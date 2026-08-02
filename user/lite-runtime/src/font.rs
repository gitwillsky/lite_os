//! Runtime proportional text: parley shaping and line breaking over the two
//! checked Noto Sans CJK SC subsets; glyph bitmap raster lives in `raster`.

mod gpu;
mod raster;
#[cfg(test)]
mod tests;

use std::{cell::RefCell, fs, io, sync::Arc};

use parley::{
    FontContext, FontData, FontFamily, FontWeight, Layout, LayoutContext, LineHeight,
    StyleProperty, TextWrapMode,
    editing::{Cursor, Selection},
    fontique::Blob,
    layout::Affinity,
};
use swash::scale::ScaleContext;
use taffy::prelude::{AvailableSpace, Size};

use crate::{renderer::SCALE, style::Computed};
pub(crate) use gpu::{GlyphAtlas, GpuTextRun};
use raster::{GlyphCache, GlyphKey};

const REGULAR_PATH: &str = "/usr/share/liteos/liteos-ui-regular.otf";
const BOLD_PATH: &str = "/usr/share/liteos/liteos-ui-bold.otf";

/// Visual movement over shaped cluster boundaries in a single-line control.
#[derive(Clone, Copy)]
pub(crate) enum CursorMove {
    Previous,
    Next,
    PreviousWord,
    NextWord,
}

/// Shaped geometry needed to paint one text-control selection.
pub(crate) struct ControlSelectionGeometry {
    pub(crate) caret_x: f32,
    pub(crate) ranges: Vec<(f32, f32)>,
}

/// One registered face: the owned font bytes handed to fontique and reused by
/// swash. The `Arc` shares one allocation between the fontique collection
/// blob and the rasterizer, so a shaped run maps back to this face by data
/// pointer identity.
struct Face {
    bytes: Arc<FaceBytes>,
}

/// Newtype so `Arc<FaceBytes>` coerces to the `Arc<dyn AsRef<[u8]>>` a
/// fontique blob wants (`Arc<[u8]>` itself is unsized and cannot).
struct FaceBytes(Box<[u8]>);

impl AsRef<[u8]> for FaceBytes {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

/// Layout-cache capacity in entries.
///
/// One desktop frame touches ~300-400 distinct layout inputs (~100 text
/// nodes × their taffy-measure/draw variants), so 512 covers the working set
/// before any eviction. A miss costs one parley shaping (hundreds of µs for
/// short UI strings); without the cache every frame re-shapes each node 3-4×,
/// which regressed the frame-timing gate to p50 ≈ 29 ms. Render-thread
/// exclusive behind the same `RefCell` discipline as `GlyphCache`.
const LAYOUT_CACHE_CAPACITY: usize = 512;

/// Full input identity of one shaped layout. Sizes/leading/advance are
/// physical pixels stored as `f32` bits (CSS sizes are exact binary
/// fractions); `advance_bits` uses `u32::MAX` as the `None` sentinel (a real
/// width is always finite, never that NaN pattern).
#[derive(Clone, PartialEq, Eq, Hash)]
struct LayoutKey {
    text: String,
    size_bits: u32,
    weight_bits: u32,
    leading_bits: u32,
    wrap: bool,
    advance_bits: u32,
}

/// Bounded LRU store of shaped parley layouts; see `LAYOUT_CACHE_CAPACITY`
/// for ownership, sizing and miss cost.
#[derive(Default)]
struct LayoutCache {
    entries: std::collections::HashMap<LayoutKey, (u64, Layout<()>)>,
    tick: u64,
}

impl LayoutCache {
    fn get(&mut self, key: &LayoutKey) -> Option<Layout<()>> {
        self.tick += 1;
        let (used, layout) = self.entries.get_mut(key)?;
        *used = self.tick;
        Some(layout.clone())
    }

    fn insert(&mut self, key: LayoutKey, layout: Layout<()>) {
        if !self.entries.contains_key(&key) && self.entries.len() >= LAYOUT_CACHE_CAPACITY {
            // Full: evict the single least-recently-used entry (one linear
            // scan, paid only once the working set overflows).
            if let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, (used, _))| *used)
                .map(|(key, _)| key.clone())
            {
                self.entries.remove(&oldest);
            }
        }
        self.tick += 1;
        self.entries.insert(key, (self.tick, layout));
    }
}

/// Runtime text engine for the proportional UI faces.
///
/// Holds the two checked Noto Sans CJK SC subsets registered with a fontique
/// collection, plus the parley/swash scratch contexts and the glyph/layout
/// caches. All contexts sit behind `RefCell` because `measure`/`draw` take
/// `&self` (callers in the paint walk hold `&Renderer`) while shaping and
/// raster mutate scratch state; the render thread is the only caller and no
/// method nests another `Font` call under a live borrow, so borrows never
/// conflict.
pub struct Font {
    regular: Face,
    bold: Face,
    /// Family name fontique parsed from the registered faces, pushed as the
    /// font stack of every layout so the collection resolves to these faces.
    family: String,
    fcx: RefCell<FontContext>,
    lcx: RefCell<LayoutContext<()>>,
    scx: RefCell<ScaleContext>,
    cache: RefCell<GlyphCache>,
    layouts: RefCell<LayoutCache>,
}

impl Font {
    /// Opens both checked UI faces and registers them with the collection.
    ///
    /// # Errors
    ///
    /// Returns an error when a face is missing, unreadable, or fontique finds
    /// no family in it (truncated or wrong file).
    pub fn open() -> io::Result<Self> {
        Self::from_faces(fs::read(REGULAR_PATH)?, fs::read(BOLD_PATH)?)
    }

    /// Registers both faces from in-memory font bytes.
    ///
    /// `pub(crate)` for renderer integration tests, which load the checked
    /// faces from `assets/fonts` instead of the guest rootfs paths.
    pub(crate) fn from_faces(regular: Vec<u8>, bold: Vec<u8>) -> io::Result<Self> {
        let regular = Face {
            bytes: Arc::new(FaceBytes(regular.into_boxed_slice())),
        };
        let bold = Face {
            bytes: Arc::new(FaceBytes(bold.into_boxed_slice())),
        };
        let mut fcx = FontContext::new();
        let mut family = None;
        for face in [&regular, &bold] {
            let registered = fcx
                .collection
                .register_fonts(Blob::new(face.bytes.clone()), None);
            let Some((family_id, _)) = registered.first() else {
                return Err(invalid("UI face registers no family"));
            };
            let name = fcx
                .collection
                .family_name(*family_id)
                .ok_or_else(|| invalid("UI face family name is missing"))?
                .to_owned();
            if let Some(previous) = &family
                && *previous != name
            {
                return Err(invalid("UI faces disagree on the family name"));
            }
            family = Some(name);
        }
        Ok(Self {
            regular,
            bold,
            family: family.expect("two faces registered"),
            fcx: RefCell::new(fcx),
            lcx: RefCell::new(LayoutContext::new()),
            scx: RefCell::new(ScaleContext::new()),
            cache: RefCell::new(GlyphCache::default()),
            layouts: RefCell::new(LayoutCache::default()),
        })
    }

    /// Logical-pixel advance of `text` laid out as a single line.
    ///
    /// Used for the `<input>` caret and any single-line intrinsic width.
    /// Shaping is real (kerning included), so the caret lands on the same
    /// advance the rasterizer draws.
    #[cfg(test)]
    pub fn measure(&self, style: &Computed, text: &str) -> f32 {
        self.single_line_width(style, text) / SCALE
    }

    /// Returns the nearest shaped cluster boundary for a physical x coordinate.
    pub(crate) fn control_cursor_from_point(&self, style: &Computed, text: &str, x: f32) -> usize {
        let layout = self.build_layout(style, text, false, None);
        Cursor::from_point(&layout, x, 0.0).index()
    }

    /// Moves a byte-index cursor through the same shaped clusters used to draw.
    pub(crate) fn move_control_cursor(
        &self,
        style: &Computed,
        text: &str,
        index: usize,
        movement: CursorMove,
    ) -> usize {
        let layout = self.build_layout(style, text, false, None);
        let cursor = Cursor::from_byte_index(&layout, index, Affinity::Downstream);
        match movement {
            CursorMove::Previous => cursor.previous_visual(&layout),
            CursorMove::Next => cursor.next_visual(&layout),
            CursorMove::PreviousWord => cursor.previous_visual_word(&layout),
            CursorMove::NextWord => cursor.next_visual_word(&layout),
        }
        .index()
    }

    /// Shapes caret and selection geometry in the control's local coordinates.
    pub(crate) fn control_selection_geometry(
        &self,
        style: &Computed,
        text: &str,
        anchor: usize,
        focus: usize,
    ) -> ControlSelectionGeometry {
        let layout = self.build_layout(style, text, false, None);
        let anchor = Cursor::from_byte_index(&layout, anchor, Affinity::Downstream);
        let focus = Cursor::from_byte_index(&layout, focus, Affinity::Downstream);
        let ranges = if anchor == focus {
            Vec::new()
        } else {
            Selection::new(anchor, focus)
                .geometry(&layout)
                .into_iter()
                .map(|(rect, _)| (rect.x0 as f32, rect.x1 as f32))
                .collect()
        };
        Self::cursor_x(&layout, focus, ranges)
    }

    fn cursor_x(
        layout: &Layout<()>,
        focus: Cursor,
        ranges: Vec<(f32, f32)>,
    ) -> ControlSelectionGeometry {
        ControlSelectionGeometry {
            caret_x: focus.geometry(layout, 0.0).x0 as f32,
            ranges,
        }
    }

    /// Physical height of one CSS line for the supplied computed text style.
    ///
    /// Replaced single-line controls use this metric to position their line
    /// box without duplicating CSS `line-height` parsing outside the font owner.
    pub(crate) fn single_line_height(&self, style: &Computed) -> f32 {
        line_height(style, style.px("font-size", 11.0) * SCALE)
    }

    /// Intrinsic logical size of a text leaf under taffy's constraints.
    ///
    /// `white-space: normal`/`pre-wrap` text wraps at a definite inline
    /// constraint (parley line breaking) and reports the resulting block
    /// height; `pre`/`nowrap` text keeps its unbroken lines. Min-content
    /// resolves to the widest unbreakable run, max-content to the full
    /// unwrapped advance. Height is the summed CSS `line-height` of the
    /// resulting lines.
    ///
    /// Benchmark decision: repeat inputs hit the layout cache
    /// (`LAYOUT_CACHE_CAPACITY`), so taffy's several per-node measure calls
    /// and `draw`'s own layout shape once per distinct (text, style, width)
    /// input instead of 3-4 times per frame — the uncached form regressed
    /// the frame-timing gate to p50 ≈ 29 ms.
    pub(crate) fn measure_text(
        &self,
        style: &Computed,
        text: &str,
        known: Size<Option<f32>>,
        available: Size<AvailableSpace>,
    ) -> Size<f32> {
        let wrap = wraps(style);
        // taffy speaks logical px; parley layouts run in physical px.
        let definite = known.width.or(match available.width {
            AvailableSpace::Definite(width) => Some(width),
            _ => None,
        });
        let layout = match definite {
            Some(width) => self.build_layout(style, text, wrap, Some(width * SCALE)),
            // Min-content: wrap at every opportunity so the widest
            // unbreakable run defines the width.
            None if wrap && available.width == AvailableSpace::MinContent => {
                self.build_layout(style, text, true, Some(0.0))
            }
            None => self.build_layout(style, text, wrap, None),
        };
        let width = if definite.is_some() || available.width == AvailableSpace::MinContent {
            layout.width()
        } else {
            layout.full_width()
        };
        Size {
            width: width / SCALE,
            height: layout.height() / SCALE,
        }
    }

    /// Builds a parley layout of `text` at the style's size/weight/leading.
    ///
    /// `max_advance` is the physical wrap/alignment width; `None` leaves
    /// lines unbroken past hard breaks. Font sizes are logical px × `SCALE`,
    /// so any CSS `font-size` rasterizes at its own outlines instead of the
    /// nearest preset. Repeat inputs hit the layout cache (see
    /// `LAYOUT_CACHE_CAPACITY`); taffy measure and `draw` ask for identical
    /// inputs several times per frame.
    fn build_layout(
        &self,
        style: &Computed,
        text: &str,
        wrap: bool,
        max_advance: Option<f32>,
    ) -> Layout<()> {
        let font_size = style.px("font-size", 11.0) * SCALE;
        let weight = font_weight(style);
        let leading = line_height(style, font_size);
        let key = LayoutKey {
            text: text.to_owned(),
            size_bits: font_size.to_bits(),
            weight_bits: weight.value().to_bits(),
            leading_bits: leading.to_bits(),
            wrap,
            advance_bits: max_advance.map_or(u32::MAX, f32::to_bits),
        };
        if let Some(layout) = self.layouts.borrow_mut().get(&key) {
            return layout;
        }
        let mut fcx = self.fcx.borrow_mut();
        let mut lcx = self.lcx.borrow_mut();
        let mut builder = lcx.ranged_builder(&mut fcx, text, 1.0, true);
        builder.push_default(StyleProperty::FontSize(font_size));
        builder.push_default(StyleProperty::FontWeight(weight));
        builder.push_default(StyleProperty::LineHeight(LineHeight::Absolute(leading)));
        builder.push_default(StyleProperty::FontFamily(FontFamily::Source(
            self.family.as_str().into(),
        )));
        if !wrap {
            builder.push_default(StyleProperty::TextWrapMode(TextWrapMode::NoWrap));
        }
        let mut layout = builder.build(text);
        layout.break_all_lines(max_advance);
        self.layouts.borrow_mut().insert(key, layout.clone());
        layout
    }

    /// Physical advance of `text` as a single unwrapped line.
    fn single_line_width(&self, style: &Computed, text: &str) -> f32 {
        self.build_layout(style, text, false, None).full_width()
    }

    /// Truncates `text` to fit `box_width` (physical px) with a trailing
    /// ellipsis. Binary search over char-boundary prefixes keeps the rare
    /// overflow path at ~log2(n) layout builds; kerning/ligature error at the
    /// cut point is sub-pixel, so advance monotonicity is close enough.
    fn ellipsize(&self, style: &Computed, text: &str, box_width: i32) -> String {
        let ellipsis_width = self.single_line_width(style, "…");
        let budget = box_width as f32 - ellipsis_width;
        if budget <= 0.0 {
            return "…".to_owned();
        }
        let boundaries: Vec<usize> = text
            .char_indices()
            .map(|(index, _)| index)
            .chain(std::iter::once(text.len()))
            .collect();
        let mut fits = 0;
        let mut rest = boundaries.len();
        while fits + 1 < rest {
            let mid = (fits + rest) / 2;
            if self.single_line_width(style, &text[..boundaries[mid]]) <= budget {
                fits = mid;
            } else {
                rest = mid;
            }
        }
        let mut kept = text[..boundaries[fits]].to_owned();
        kept.push('…');
        kept
    }

    /// Maps a shaped run's font back to the bold face by blob identity.
    /// fontique only ever returns the two blobs registered in `from_faces`,
    /// so a mismatch is impossible; it still falls back to regular rather
    /// than panicking (a wrong face renders the right text at the wrong
    /// weight).
    fn is_bold(&self, font: &FontData) -> bool {
        let data: &[u8] = font.data.as_ref();
        data.as_ptr() == self.bold.bytes.0.as_ptr() && data.len() == self.bold.bytes.0.len()
    }
}

/// `white-space: normal` (the CSS initial value, so also an absent property)
/// and `pre-wrap` wrap at the inline constraint; `pre` and `nowrap` keep
/// their lines unbroken.
fn wraps(style: &Computed) -> bool {
    !matches!(style.get("white-space"), Some("pre" | "nowrap"))
}

/// CSS `font-weight` as a fontique weight. Only the 400 and 700 faces are
/// registered; CSS Fonts 4 matching resolves 500 to regular and 600+ to bold,
/// so intermediate weights degrade to the nearest checked face.
fn font_weight(style: &Computed) -> FontWeight {
    let weight = match style.get("font-weight") {
        Some("bold") => 700.0,
        Some("normal") | None => 400.0,
        Some(value) => value.parse().unwrap_or(400.0),
    };
    FontWeight::new(weight)
}

/// Physical CSS `line-height`: `px` values scale, a unitless number
/// multiplies the font size, and a percentage resolves against the font
/// size. `normal` (or an absent property) keeps the LiteUI UA default of
/// 1.25× the font size, matching the previous atlas line metrics.
fn line_height(style: &Computed, font_size: f32) -> f32 {
    let Some(value) = style.get("line-height") else {
        return font_size * 1.25;
    };
    let value = value.trim();
    if let Some(percent) = value.strip_suffix('%') {
        return percent.trim().parse::<f32>().unwrap_or(125.0) / 100.0 * font_size;
    }
    if let Some(px) = value.strip_suffix("px") {
        return px.trim().parse::<f32>().unwrap_or(font_size * 1.25 / SCALE) * SCALE;
    }
    if value == "normal" {
        return font_size * 1.25;
    }
    value.parse::<f32>().unwrap_or(1.25) * font_size
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
