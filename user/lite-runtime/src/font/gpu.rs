//! Persistent A8 glyph atlas and GPU text command geometry.

use std::collections::HashMap;

use display_proto::{Glyph, Rect, Size};
use parley::{Alignment, AlignmentOptions, layout::PositionedLayoutItem};

use super::{FaceKind, Font, GlyphKey, wraps};
use crate::{renderer::PhysicalRect, style::Computed};

const ATLAS_WIDTH: usize = 2048;
const MAX_ATLAS_GLYPHS: usize = 4096;

/// Stable raster identity inside one renderer-owned atlas.
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub(crate) enum AtlasKey {
    Ui {
        face: FaceKind,
        glyph: u32,
        size_bits: u32,
        italic: bool,
    },
    Terminal {
        bold: bool,
        glyph: u32,
    },
}

/// One colored glyph run ready for the display-list protocol.
pub(crate) struct GpuTextRun {
    pub(crate) color: u32,
    pub(crate) glyphs: Vec<Glyph>,
}

/// One renderer's tightly packed A8 glyph coverage texture.
#[derive(Default)]
pub(crate) struct GlyphAtlas {
    bytes: Vec<u8>,
    x: usize,
    y: usize,
    row_height: usize,
    placements: HashMap<AtlasKey, Rect>,
    dirty: bool,
}

impl GlyphAtlas {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Starts one paint pass and bounds glyph storage across an unbounded app lifetime.
    pub(crate) fn begin_frame(&mut self) {
        if self.placements.len() < MAX_ATLAS_GLYPHS {
            return;
        }
        // The current document is repacked immediately after this reset. Without
        // the bound, visiting new CJK content forever would make both guest RAM
        // and the next immutable texture upload grow without limit.
        self.bytes.clear();
        self.x = 0;
        self.y = 0;
        self.row_height = 0;
        self.placements.clear();
        self.dirty = true;
    }

    /// Reuses or packs one stable bitmap and returns its source rectangle.
    pub(crate) fn insert(
        &mut self,
        key: AtlasKey,
        width: usize,
        height: usize,
        data: &[u8],
    ) -> Option<Rect> {
        if let Some(source) = self.placements.get(&key) {
            return Some(*source);
        }
        if width == 0 || height == 0 || width > ATLAS_WIDTH || data.len() != width * height {
            return None;
        }
        if self.x + width > ATLAS_WIDTH {
            self.y = self.y.checked_add(self.row_height)?;
            self.x = 0;
            self.row_height = 0;
        }
        let bottom = self.y.checked_add(height)?;
        let required = bottom.checked_mul(ATLAS_WIDTH)?;
        if required > self.bytes.len() {
            self.bytes.resize(required, 0);
        }
        for row in 0..height {
            let destination = (self.y + row) * ATLAS_WIDTH + self.x;
            self.bytes[destination..destination + width]
                .copy_from_slice(&data[row * width..(row + 1) * width]);
        }
        let source = Rect {
            x: self.x as i32,
            y: self.y as i32,
            width: width as u32,
            height: height as u32,
        };
        self.x += width;
        self.row_height = self.row_height.max(height);
        self.placements.insert(key, source);
        self.dirty = true;
        Some(source)
    }

    pub(crate) fn dirty(&self) -> bool {
        self.dirty
    }

    pub(crate) fn mark_clean(&mut self) {
        self.dirty = false;
    }

    pub(crate) fn upload(&self) -> Option<(Size, Vec<u8>)> {
        let height = self.y.checked_add(self.row_height)?;
        (height != 0).then_some((
            Size {
                width: ATLAS_WIDTH as u32,
                height: height as u32,
            },
            self.bytes.clone(),
        ))
    }
}

impl Font {
    /// Shapes proportional text and appends its glyph coverage to `atlas`.
    pub(crate) fn gpu_text(
        &self,
        atlas: &mut GlyphAtlas,
        bounds: PhysicalRect,
        overflow_clip: Option<PhysicalRect>,
        style: &Computed,
        text: &str,
    ) -> Vec<GpuTextRun> {
        if bounds.is_empty() || text.is_empty() {
            return Vec::new();
        }
        let wrap = wraps(style);
        let box_width = bounds.x2.saturating_sub(bounds.x1) as i32;
        let mut layout = self.build_layout(style, text, wrap, Some(box_width as f32));
        if !wrap
            && style.get("text-overflow") == Some("ellipsis")
            && layout.width() > box_width as f32
            && box_width > 0
        {
            let text = self.ellipsize(style, text, box_width);
            layout = self.build_layout(style, &text, false, Some(box_width as f32));
        }
        layout.align(
            match style.get("text-align") {
                Some("center") => Alignment::Center,
                Some("right") => Alignment::Right,
                _ => Alignment::Left,
            },
            AlignmentOptions::default(),
        );
        let clip = overflow_clip.map_or(bounds, |clip| bounds.intersect(clip));
        let color = style
            .get("color")
            .and_then(crate::color::parse)
            .unwrap_or(0xff00_0000);
        let italic = style.get("font-style") == Some("italic");
        let mut glyphs = Vec::new();
        for line in layout.lines() {
            for item in line.items() {
                let PositionedLayoutItem::GlyphRun(run) = item else {
                    continue;
                };
                let face = self.face_kind(run.run().font());
                let size = run.run().font_size();
                for glyph in run.positioned_glyphs() {
                    let pen = (bounds.x1 as f32 + glyph.x).round() as i32;
                    let baseline = (bounds.y1 as f32 + glyph.y).round() as i32;
                    self.gpu_glyph(
                        atlas,
                        &mut glyphs,
                        clip,
                        GlyphKey {
                            face,
                            glyph: glyph.id,
                            size_bits: size.to_bits(),
                        },
                        pen,
                        baseline,
                        italic,
                    );
                }
            }
        }
        glyphs
            .chunks(display_proto::MAX_GLYPHS_PER_RUN)
            .map(|glyphs| GpuTextRun {
                color,
                glyphs: glyphs.to_vec(),
            })
            .collect()
    }

    /// Shapes one unwrapped form-control line at an explicit physical origin.
    ///
    /// The origin may be negative while horizontal input scrolling keeps the
    /// visible glyph quads clipped to `clip`. The returned mask geometry is
    /// consumed by the compositor GPU; this method never blends destination
    /// pixels in the runtime.
    pub(crate) fn gpu_control_text(
        &self,
        atlas: &mut GlyphAtlas,
        origin: (i32, i32),
        clip: PhysicalRect,
        style: &Computed,
        text: &str,
    ) -> Vec<GpuTextRun> {
        if clip.is_empty() || text.is_empty() {
            return Vec::new();
        }
        let layout = self.build_layout(style, text, false, None);
        let color = style
            .get("color")
            .and_then(crate::color::parse)
            .unwrap_or(0xff00_0000);
        let italic = style.get("font-style") == Some("italic");
        let mut glyphs = Vec::new();
        for line in layout.lines() {
            for item in line.items() {
                let PositionedLayoutItem::GlyphRun(run) = item else {
                    continue;
                };
                let face = self.face_kind(run.run().font());
                let size = run.run().font_size();
                for glyph in run.positioned_glyphs() {
                    self.gpu_glyph(
                        atlas,
                        &mut glyphs,
                        clip,
                        GlyphKey {
                            face,
                            glyph: glyph.id,
                            size_bits: size.to_bits(),
                        },
                        (origin.0 as f32 + glyph.x).round() as i32,
                        (origin.1 as f32 + glyph.y).round() as i32,
                        italic,
                    );
                }
            }
        }
        glyphs
            .chunks(display_proto::MAX_GLYPHS_PER_RUN)
            .map(|glyphs| GpuTextRun {
                color,
                glyphs: glyphs.to_vec(),
            })
            .collect()
    }

    #[allow(clippy::too_many_arguments)]
    fn gpu_glyph(
        &self,
        atlas: &mut GlyphAtlas,
        output: &mut Vec<Glyph>,
        clip: PhysicalRect,
        key: GlyphKey,
        pen: i32,
        baseline: i32,
        italic: bool,
    ) {
        let mut cache = self.cache.borrow_mut();
        if cache.get(key).is_none() {
            drop(cache);
            let face = self.face(key.face);
            let Some(rasterized) =
                super::raster::rasterize(&mut self.scx.borrow_mut(), &face.bytes.0, key)
            else {
                return;
            };
            cache = self.cache.borrow_mut();
            cache.insert(key, rasterized);
        }
        let glyph = cache.get(key).expect("glyph inserted");
        let (width, x_offset) = oblique_geometry(glyph, italic);
        let atlas_key = AtlasKey::Ui {
            face: key.face,
            glyph: key.glyph,
            size_bits: key.size_bits,
            italic,
        };
        let bitmap = italic.then(|| oblique_bitmap(glyph));
        let data = bitmap.as_deref().unwrap_or(&glyph.data);
        let Some(source) = atlas.insert(atlas_key, width, glyph.height as usize, data) else {
            return;
        };
        let destination = Rect {
            x: pen + glyph.left + x_offset,
            y: baseline - glyph.top,
            width: width as u32,
            height: glyph.height,
        };
        if let Some((source, destination)) = crop(source, destination, clip) {
            output.push(Glyph {
                source,
                destination,
            });
        }
    }
}

fn oblique_geometry(glyph: &super::raster::CachedGlyph, italic: bool) -> (usize, i32) {
    if !italic {
        return (glyph.width as usize, 0);
    }
    let offsets = (0..glyph.height as i32)
        .map(|row| (glyph.top - row) / 5)
        .collect::<Vec<_>>();
    let minimum = offsets.iter().copied().min().unwrap_or(0);
    let maximum = offsets.iter().copied().max().unwrap_or(0);
    (glyph.width as usize + (maximum - minimum) as usize, minimum)
}

fn oblique_bitmap(glyph: &super::raster::CachedGlyph) -> Vec<u8> {
    let offsets = (0..glyph.height as i32)
        .map(|row| (glyph.top - row) / 5)
        .collect::<Vec<_>>();
    let minimum = offsets.iter().copied().min().unwrap_or(0);
    let maximum = offsets.iter().copied().max().unwrap_or(0);
    let width = glyph.width as usize + (maximum - minimum) as usize;
    let mut output = vec![0; width * glyph.height as usize];
    for (row, offset) in offsets.into_iter().enumerate() {
        let x = (offset - minimum) as usize;
        output[row * width + x..row * width + x + glyph.width as usize].copy_from_slice(
            &glyph.data[row * glyph.width as usize..(row + 1) * glyph.width as usize],
        );
    }
    output
}

fn crop(source: Rect, destination: Rect, clip: PhysicalRect) -> Option<(Rect, Rect)> {
    let x1 = destination.x.max(clip.x1 as i32);
    let y1 = destination.y.max(clip.y1 as i32);
    let x2 = destination
        .x
        .saturating_add_unsigned(destination.width)
        .min(clip.x2 as i32);
    let y2 = destination
        .y
        .saturating_add_unsigned(destination.height)
        .min(clip.y2 as i32);
    if x2 <= x1 || y2 <= y1 {
        return None;
    }
    let dx = x1 - destination.x;
    let dy = y1 - destination.y;
    let width = (x2 - x1) as u32;
    let height = (y2 - y1) as u32;
    Some((
        Rect {
            x: source.x + dx,
            y: source.y + dy,
            width,
            height,
        },
        Rect {
            x: x1,
            y: y1,
            width,
            height,
        },
    ))
}
