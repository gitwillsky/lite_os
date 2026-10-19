//! Terminal A8 atlas -> frame-local GPU glyph list; atlas parsing lives in `terminal-font`.

use terminal_font::{BITMAP_WIDTH, CELL_HEIGHT, CELL_WIDTH, TerminalFont};
use unicode_width::UnicodeWidthChar;

use crate::{
    font::{AtlasKey, GlyphAtlas},
    renderer::PhysicalRect,
    style::Computed,
};

/// Appends fixed-cell glyph coverage to the frame-local GPU atlas.
pub(crate) fn gpu_text(
    font: &TerminalFont,
    atlas: &mut GlyphAtlas,
    bounds: PhysicalRect,
    overflow_clip: Option<PhysicalRect>,
    style: &Computed,
    text: &str,
) -> Vec<display_proto::Glyph> {
    let clip = overflow_clip.map_or(bounds, |clip| bounds.intersect(clip));
    let bold = style.get("font-weight") == Some("bold")
        || style
            .get("font-weight")
            .and_then(|value| value.parse::<u32>().ok())
            .is_some_and(|weight| weight >= 600);
    let mut column = 0usize;
    let mut output = Vec::new();
    for character in text.chars() {
        let destination_x = bounds.x1 + column * CELL_WIDTH;
        if destination_x >= bounds.x2 {
            break;
        }
        let glyph = font.glyph(character);
        let Some(source) = atlas.insert(
            AtlasKey::Terminal {
                bold,
                glyph: glyph as u32,
            },
            BITMAP_WIDTH,
            CELL_HEIGHT,
            font.bitmap(glyph, bold),
        ) else {
            break;
        };
        let destination = display_proto::Rect {
            x: destination_x as i32,
            y: bounds.y1 as i32,
            width: BITMAP_WIDTH as u32,
            height: CELL_HEIGHT as u32,
        };
        if let Some((source, destination)) = crop_glyph(source, destination, clip) {
            output.push(display_proto::Glyph {
                source,
                destination,
            });
        }
        column += UnicodeWidthChar::width(character).unwrap_or(1).clamp(1, 2);
    }
    output
}

fn crop_glyph(
    source: display_proto::Rect,
    destination: display_proto::Rect,
    clip: PhysicalRect,
) -> Option<(display_proto::Rect, display_proto::Rect)> {
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
    (x2 > x1 && y2 > y1).then(|| {
        let width = (x2 - x1) as u32;
        let height = (y2 - y1) as u32;
        (
            display_proto::Rect {
                x: source.x + x1 - destination.x,
                y: source.y + y1 - destination.y,
                width,
                height,
            },
            display_proto::Rect {
                x: x1,
                y: y1,
                width,
                height,
            },
        )
    })
}
