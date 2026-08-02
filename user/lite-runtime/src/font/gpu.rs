//! Frame-local A8 glyph atlas and GPU text command geometry.

use display_proto::{Glyph, Rect, Size};
use parley::{Alignment, AlignmentOptions, layout::PositionedLayoutItem};

use super::{Font, GlyphKey, wraps};
use crate::{renderer::PhysicalRect, style::Computed};

const ATLAS_WIDTH: usize = 2048;

/// One colored glyph run ready for the display-list protocol.
pub(crate) struct GpuTextRun {
    pub(crate) color: u32,
    pub(crate) glyphs: Vec<Glyph>,
}

/// One frame's tightly packed A8 glyph coverage texture.
pub(crate) struct GlyphAtlas {
    bytes: Vec<u8>,
    x: usize,
    y: usize,
    row_height: usize,
}

impl GlyphAtlas {
    pub(crate) fn new() -> Self {
        Self {
            bytes: Vec::new(),
            x: 0,
            y: 0,
            row_height: 0,
        }
    }

    /// Packs one bitmap and returns its source rectangle.
    pub(crate) fn insert(&mut self, width: usize, height: usize, data: &[u8]) -> Option<Rect> {
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
        Some(source)
    }

    pub(crate) fn finish(self) -> Option<(Size, Vec<u8>)> {
        let height = self.y.checked_add(self.row_height)?;
        (height != 0).then_some((
            Size {
                width: ATLAS_WIDTH as u32,
                height: height as u32,
            },
            self.bytes,
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
                let bold = self.is_bold(run.run().font());
                let size = run.run().font_size();
                for glyph in run.positioned_glyphs() {
                    let pen = (bounds.x1 as f32 + glyph.x).round() as i32;
                    let baseline = (bounds.y1 as f32 + glyph.y).round() as i32;
                    self.gpu_glyph(
                        atlas,
                        &mut glyphs,
                        clip,
                        GlyphKey {
                            bold,
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
                let bold = self.is_bold(run.run().font());
                let size = run.run().font_size();
                for glyph in run.positioned_glyphs() {
                    self.gpu_glyph(
                        atlas,
                        &mut glyphs,
                        clip,
                        GlyphKey {
                            bold,
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
            let face = if key.bold { &self.bold } else { &self.regular };
            let Some(rasterized) =
                super::raster::rasterize(&mut self.scx.borrow_mut(), &face.bytes.0, key)
            else {
                return;
            };
            cache = self.cache.borrow_mut();
            cache.insert(key, rasterized);
        }
        let glyph = cache.get(key).expect("glyph inserted");
        let (bitmap, width, x_offset) = oblique_bitmap(glyph, italic);
        let Some(source) = atlas.insert(width, glyph.height as usize, &bitmap) else {
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

fn oblique_bitmap(glyph: &super::raster::CachedGlyph, italic: bool) -> (Vec<u8>, usize, i32) {
    if !italic {
        return (glyph.data.clone(), glyph.width as usize, 0);
    }
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
    (output, width, minimum)
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
