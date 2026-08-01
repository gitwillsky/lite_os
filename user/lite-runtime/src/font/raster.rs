//! A8 glyph bitmap raster (swash) and its bounded LRU cache for the UI font.

use std::collections::HashMap;

use swash::{
    FontRef,
    scale::{Render, ScaleContext, Source},
};

use crate::renderer::{PhysicalRect, Raster};

/// Glyph bitmap cache capacity in entries.
///
/// One desktop frame draws well under 1000 distinct (face, glyph, size)
/// triples — a file-manager window peaks near 400 — so 2048 covers two
/// CJK-heavy frames before any eviction. A miss costs one swash outline
/// raster (tens of µs) on the paint path; without the cache every glyph of
/// every frame would pay it. Render-thread exclusive (`Renderer` owns the
/// only `Font`), so a plain `RefCell` guard on the owner is sufficient; a
/// poisoned or contended borrow cannot occur because no `Font` method is
/// reentered while a borrow is live.
const GLYPH_CACHE_CAPACITY: usize = 2048;

/// Weight/size key for one cached glyph bitmap. Sizes are physical pixels
/// (logical px × `SCALE`) stored as `f32` bits: CSS sizes are exact binary
/// fractions, so bit equality matches cache equality.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct GlyphKey {
    pub(super) bold: bool,
    pub(super) glyph: u32,
    pub(super) size_bits: u32,
}

/// One rasterized A8 glyph bitmap and its placement relative to the pen
/// origin: the bitmap's top-left pixel lands at `(pen.x + left, baseline -
/// top)`.
pub(super) struct CachedGlyph {
    left: i32,
    top: i32,
    width: u32,
    height: u32,
    data: Vec<u8>,
    /// Last use tick; the oldest entry is evicted when the cache is full.
    used: u64,
}

/// LRU glyph bitmap store. Owned by `Font` behind a `RefCell`; see
/// `GLYPH_CACHE_CAPACITY` for ownership, sizing and miss cost.
#[derive(Default)]
pub(super) struct GlyphCache {
    entries: HashMap<GlyphKey, CachedGlyph>,
    tick: u64,
}

impl GlyphCache {
    pub(super) fn get(&mut self, key: GlyphKey) -> Option<&CachedGlyph> {
        self.tick += 1;
        let entry = self.entries.get_mut(&key)?;
        entry.used = self.tick;
        Some(entry)
    }

    pub(super) fn insert(&mut self, key: GlyphKey, mut glyph: CachedGlyph) -> &CachedGlyph {
        if !self.entries.contains_key(&key) && self.entries.len() >= GLYPH_CACHE_CAPACITY {
            // Full: evict the single least-recently-used entry. The linear
            // scan costs ~2048 comparisons, paid once per new glyph after
            // warm-up (steady state evicts nothing, the working set fits).
            if let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.used)
                .map(|(key, _)| *key)
            {
                self.entries.remove(&oldest);
            }
        }
        self.tick += 1;
        glyph.used = self.tick;
        self.entries.entry(key).or_insert(glyph)
    }
}

/// Rasterizes one glyph outline to an A8 bitmap at its cached size. Returns
/// `None` for a glyph id the face cannot scale (out of range or malformed
/// outline); the caller skips the glyph, matching a missing-glyph .notdef
/// that itself has no drawable outline.
pub(super) fn rasterize(
    scx: &mut ScaleContext,
    face_bytes: &[u8],
    key: GlyphKey,
) -> Option<CachedGlyph> {
    let font = FontRef::from_index(face_bytes, 0)?;
    // No hinting: the checked subsets ship unhinted CFF outlines (see
    // scripts/generate_ui_font.py), so there is nothing to hint with.
    let mut scaler = scx
        .builder(font)
        .size(f32::from_bits(key.size_bits))
        .hint(false)
        .build();
    let image = Render::new(&[Source::Outline]).render(&mut scaler, key.glyph as u16)?;
    Some(CachedGlyph {
        left: image.placement.left,
        top: image.placement.top,
        width: image.placement.width,
        height: image.placement.height,
        data: image.data,
        used: 0,
    })
}

/// Paints one cached A8 glyph into the target, clipped to `clip`.
pub(super) fn blit<R: Raster>(
    target: &mut R,
    clip: PhysicalRect,
    glyph: &CachedGlyph,
    pen: i32,
    baseline: i32,
    color: u32,
    italic: bool,
) {
    let x = pen + glyph.left;
    let y = baseline - glyph.top;
    for row in 0..glyph.height as i32 {
        let target_y = y + row;
        if target_y < clip.y1 as i32 || target_y >= clip.y2 as i32 {
            continue;
        }
        // Synthetic oblique for `font-style: italic`: rows lean right in
        // proportion to their height above the baseline (~11°) while the
        // upright face's advances and metrics stay unchanged.
        let lean = if italic { (baseline - target_y) / 5 } else { 0 };
        let target_row = target.row_mut(target_y as usize);
        for column in 0..glyph.width as i32 {
            let target_x = x + lean + column;
            if target_x < clip.x1 as i32 || target_x >= clip.x2 as i32 {
                continue;
            }
            let alpha = glyph.data[row as usize * glyph.width as usize + column as usize];
            if alpha != 0 {
                let background = target_row[target_x as usize];
                target_row[target_x as usize] = blend(background, color, alpha);
            }
        }
    }
}

fn blend(background: u32, foreground: u32, alpha: u8) -> u32 {
    let alpha = u32::from(alpha);
    let inverse = 255 - alpha;
    let channel = |shift: u32| {
        (((foreground >> shift) & 0xffu32) * alpha + ((background >> shift) & 0xffu32) * inverse)
            / 255
    };
    0xff00_0000 | channel(16) << 16 | channel(8) << 8 | channel(0)
}
