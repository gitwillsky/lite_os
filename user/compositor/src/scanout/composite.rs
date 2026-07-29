//! Pure scene-composition geometry: node clipping, damage unions and the
//! premultiplied OVER operator shared by scanout composition and the cursor.

use display_proto::{Rect, SceneNodeKind};
use linux_uapi::drm::{Clip, DumbBuffer};

pub(super) fn composite_node(
    target: &mut DumbBuffer,
    source: &DumbBuffer,
    node: &crate::session::Node,
    screen: Rect,
    damage: Rect,
    offset: (i32, i32),
) {
    let bounds = translated(node.bounds, offset);
    let clip = translated(node.clip, offset);
    let x1 = bounds.x.max(clip.x).max(screen.x).max(0);
    let x1 = x1.max(damage.x);
    let y1 = bounds.y.max(clip.y).max(screen.y).max(0).max(damage.y);
    let x2 = (bounds.x + bounds.width as i32)
        .min(clip.x + clip.width as i32)
        .min(screen.width as i32)
        .min(damage.x.saturating_add_unsigned(damage.width));
    let y2 = (bounds.y + bounds.height as i32)
        .min(clip.y + clip.height as i32)
        .min(screen.height as i32)
        .min(damage.y.saturating_add_unsigned(damage.height));
    if x2 <= x1 || y2 <= y1 {
        return;
    }
    // Rounded corners: rows within `corner_radius` of the clip top inset
    // horizontally so the frame clip skips the top corner cutout, letting
    // lower content show through instead of being covered by stale chrome
    // pixels. Chrome and windows are both `8px 8px 0 0` (top-only), so only
    // the top edge rounds; the bottom stays square. The inset math mirrors the
    // renderer's `corner_inset` so the clip edge aligns with the painted arc.
    let r = node.corner_radius as f32;
    let r_sq = r * r;
    for y in y1..y2 {
        let source_y = (y - bounds.y) as usize;
        let source_row = source.row(source_y);
        let target_row = target.row_mut(y as usize);
        let (mut px1, mut px2) = (x1, x2);
        if node.corner_radius > 0 {
            // Distance in rows from the top clip edge; only rows inside the top
            // corner region get inset.
            let edge_dist = y - clip.y;
            if edge_dist >= 0 && (edge_dist as f32) < r {
                let mid = edge_dist as f32 + 0.5;
                let dist = r - mid;
                let inset = r - (r_sq - dist * dist).max(0.0).sqrt();
                let left_px = (clip.x as f32 + inset).ceil() as i32;
                if px1 < left_px {
                    px1 = left_px;
                }
                let right_px = (clip.x as f32 + clip.width as f32 - inset).floor() as i32;
                if px2 > right_px {
                    px2 = right_px;
                }
            }
        }
        if px2 <= px1 {
            continue;
        }
        let opaque = node.opaque.map(|rectangle| translated(rectangle, offset));
        if opaque.is_some_and(|opaque| {
            y >= opaque.y
                && y < opaque.y.saturating_add_unsigned(opaque.height)
                && px1 >= opaque.x
                && px2 <= opaque.x.saturating_add_unsigned(opaque.width)
        }) {
            let source_start = (px1 - bounds.x) as usize;
            let source_end = (px2 - bounds.x) as usize;
            target_row[px1 as usize..px2 as usize]
                .copy_from_slice(&source_row[source_start..source_end]);
            continue;
        }
        for x in px1..px2 {
            let source_pixel = source_row[(x - bounds.x) as usize];
            target_row[x as usize] = over(source_pixel, target_row[x as usize]);
        }
    }
}

pub(super) fn clear(target: &mut DumbBuffer, rectangle: Rect) {
    let x1 = rectangle.x as usize;
    let x2 = x1 + rectangle.width as usize;
    for y in rectangle.y as usize..rectangle.y as usize + rectangle.height as usize {
        target.row_mut(y)[x1..x2].fill(0);
    }
}

pub(super) fn translated(rectangle: Rect, offset: (i32, i32)) -> Rect {
    Rect {
        x: rectangle.x.saturating_add(offset.0),
        y: rectangle.y.saturating_add(offset.1),
        ..rectangle
    }
}

pub(super) fn group_bounds(nodes: &[crate::session::Node], window_group: u32) -> Option<Rect> {
    nodes
        .iter()
        .filter(|node| node.window_group == window_group)
        .filter_map(|node| intersect(node.bounds, node.clip))
        .reduce(union)
}

/// Damage a full compose must repaint for the moving group's temporary
/// transform: the canonical scene bounds, the bounds translated by the
/// current offset, and the stale rect this buffer was last painted at.
///
/// A direct front-buffer move is not represented by scene revisions, so none
/// of these comes from the revision-diff damage; missing the stale rect lets
/// a concurrently submitted scene flip with the previous temporary position
/// still painted, and no later damage ever covers it (the fast-drag trails
/// seen whenever a periodically committing app like the music player shares
/// the screen with a dragged window).
pub(super) fn moving_group_damage(
    canonical: Option<Rect>,
    offset: (i32, i32),
    stale: Option<Rect>,
) -> Option<Rect> {
    [
        canonical,
        canonical.map(|bounds| translated(bounds, offset)),
        stale,
    ]
    .into_iter()
    .flatten()
    .reduce(union)
}

pub(super) fn source_buffer_id(
    node: &crate::session::Node,
    active_move: Option<(u32, (i32, i32), u32)>,
) -> u32 {
    active_move.map_or(node.buffer_id, |(window_group, _, underlay)| {
        if node.kind == SceneNodeKind::Pixels && node.window_group != window_group {
            underlay
        } else {
            node.buffer_id
        }
    })
}

pub(super) fn intersect(left: Rect, right: Rect) -> Option<Rect> {
    let x1 = left.x.max(right.x);
    let y1 = left.y.max(right.y);
    let x2 = left
        .x
        .saturating_add_unsigned(left.width)
        .min(right.x.saturating_add_unsigned(right.width));
    let y2 = left
        .y
        .saturating_add_unsigned(left.height)
        .min(right.y.saturating_add_unsigned(right.height));
    (x2 > x1 && y2 > y1).then_some(Rect {
        x: x1,
        y: y1,
        width: (x2 - x1) as u32,
        height: (y2 - y1) as u32,
    })
}

pub(super) fn union(left: Rect, right: Rect) -> Rect {
    let x1 = left.x.min(right.x);
    let y1 = left.y.min(right.y);
    let x2 = left
        .x
        .saturating_add_unsigned(left.width)
        .max(right.x.saturating_add_unsigned(right.width));
    let y2 = left
        .y
        .saturating_add_unsigned(left.height)
        .max(right.y.saturating_add_unsigned(right.height));
    Rect {
        x: x1,
        y: y1,
        width: x2.saturating_sub(x1) as u32,
        height: y2.saturating_sub(y1) as u32,
    }
}

pub(super) fn to_clip(rectangle: Rect) -> Clip {
    Clip {
        x1: rectangle.x as u16,
        y1: rectangle.y as u16,
        x2: rectangle.x.saturating_add_unsigned(rectangle.width) as u16,
        y2: rectangle.y.saturating_add_unsigned(rectangle.height) as u16,
    }
}

pub(super) fn valid_clip(clip: &Clip) -> bool {
    clip.x2 > clip.x1 && clip.y2 > clip.y1
}

pub(super) fn from_clip(clip: Clip) -> Option<Rect> {
    valid_clip(&clip).then_some(Rect {
        x: i32::from(clip.x1),
        y: i32::from(clip.y1),
        width: u32::from(clip.x2 - clip.x1),
        height: u32::from(clip.y2 - clip.y1),
    })
}

/// Composites one premultiplied ARGB8888 `source` pixel over `destination`.
///
/// Porter-Duff OVER for premultiplied source: `out = source + dest * (1 - a)`.
/// The `source` color channels must already be scaled by its alpha — straight
/// alpha would double-count the coverage and render translucent edges too bright.
/// The result carries no alpha (the scanout buffer is presented as XRGB8888).
///
/// Shared with the cursor overlay ([`crate::cursor`]), which alpha-blends its
/// RGBA shape pixels through the same operator so rounding stays identical.
pub(crate) fn over(source: u32, destination: u32) -> u32 {
    let alpha = source >> 24;
    if alpha == 255 {
        return source & 0x00ff_ffff;
    }
    if alpha == 0 {
        return destination;
    }
    let inverse = 255 - alpha;
    let red = ((source >> 16) & 0xff) + (((destination >> 16) & 0xff) * inverse + 127) / 255;
    let green = ((source >> 8) & 0xff) + (((destination >> 8) & 0xff) * inverse + 127) / 255;
    let blue = (source & 0xff) + ((destination & 0xff) * inverse + 127) / 255;
    (red.min(255) << 16) | (green.min(255) << 8) | blue.min(255)
}
