//! Pure scene-composition geometry: node clipping, damage unions and the
//! premultiplied OVER operator shared by scanout composition and the cursor.

use display_proto::{ClipMask, CornerRadius, Rect, SceneNodeKind};
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
    for y in y1..y2 {
        let source_y = (y - bounds.y) as usize;
        let source_row = source.row(source_y);
        let target_row = target.row_mut(y as usize);
        let (mut px1, mut px2) = (x1, x2);
        let mut rounded = (clip.x as f32, (clip.x + clip.width as i32) as f32);
        for mask in &node.clip_masks {
            let mask = ClipMask {
                rect: translated(mask.rect, offset),
                ..*mask
            };
            let Some(span) = rounded_span(mask, y) else {
                px2 = px1;
                break;
            };
            rounded.0 = rounded.0.max(span.0);
            rounded.1 = rounded.1.min(span.1);
        }
        px1 = px1.max(rounded.0.floor() as i32);
        px2 = px2.min(rounded.1.ceil() as i32);
        if px2 <= px1 {
            continue;
        }
        let opaque = node.opaque.map(|rectangle| translated(rectangle, offset));
        let fully_covered = rounded.0.fract() == 0.0 && rounded.1.fract() == 0.0;
        if fully_covered
            && opaque.is_some_and(|opaque| {
                y >= opaque.y
                    && y < opaque.y.saturating_add_unsigned(opaque.height)
                    && px1 >= opaque.x
                    && px2 <= opaque.x.saturating_add_unsigned(opaque.width)
            })
        {
            let source_start = (px1 - bounds.x) as usize;
            let source_end = (px2 - bounds.x) as usize;
            target_row[px1 as usize..px2 as usize]
                .copy_from_slice(&source_row[source_start..source_end]);
            continue;
        }
        for x in px1..px2 {
            let coverage =
                (rounded.1.min(x as f32 + 1.0) - rounded.0.max(x as f32)).clamp(0.0, 1.0);
            let source_pixel = scale_pm(source_row[(x - bounds.x) as usize], coverage);
            target_row[x as usize] = over(source_pixel, target_row[x as usize]);
        }
    }
}

fn rounded_span(mask: ClipMask, y: i32) -> Option<(f32, f32)> {
    let row = y - mask.rect.y;
    let height = mask.rect.height as i32;
    if row < 0 || row >= height {
        return None;
    }
    let width = mask.rect.width;
    let inset = |top: CornerRadius, bottom: CornerRadius| {
        let normalize = |radius: CornerRadius| CornerRadius {
            x: radius.x.min(width / 2),
            y: radius.y.min(mask.rect.height / 2),
        };
        let top = normalize(top);
        let bottom = normalize(bottom);
        let mid = row as f32 + 0.5;
        let ellipse = |radius: CornerRadius, distance: f32| {
            if radius.x == 0 || radius.y == 0 {
                return 0.0;
            }
            let normalized = distance / radius.y as f32;
            radius.x as f32
                * (1.0 - (1.0 - normalized * normalized).max(0.0).sqrt())
        };
        if row < top.y as i32 {
            ellipse(top, top.y as f32 - mid)
        } else if row >= height - bottom.y as i32 {
            ellipse(bottom, mid - (height as f32 - bottom.y as f32))
        } else {
            0.0
        }
    };
    let left = inset(mask.radii[0], mask.radii[3]);
    let right = inset(mask.radii[1], mask.radii[2]);
    let x1 = mask.rect.x as f32 + left;
    let x2 = mask.rect.x as f32 + mask.rect.width as f32 - right;
    if x2 > x1 {
        Some((x1, x2))
    } else {
        None
    }
}

fn scale_pm(color: u32, coverage: f32) -> u32 {
    let channel = |shift: u32| (((color >> shift) & 0xff) as f32 * coverage).round() as u32;
    channel(24) << 24 | channel(16) << 16 | channel(8) << 8 | channel(0)
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

#[cfg(test)]
mod tests {
    use display_proto::{ClipMask, CornerRadius, Rect};

    #[test]
    fn rounded_scene_clip_is_symmetric_across_all_four_corners() {
        let mask = ClipMask {
            rect: Rect {
                x: 10,
                y: 20,
                width: 20,
                height: 12,
            },
            radii: [CornerRadius { x: 4, y: 4 }; 4],
        };
        let top = super::rounded_span(mask, 20).expect("top row");
        let bottom = super::rounded_span(mask, 31).expect("bottom row");
        let upper_inner = super::rounded_span(mask, 21).expect("upper row");
        let lower_inner = super::rounded_span(mask, 30).expect("lower row");

        assert_eq!(top, bottom, "top and bottom outer arcs must match");
        assert_eq!(
            upper_inner, lower_inner,
            "top and bottom inner arcs must match"
        );
        assert!(
            top.0 > mask.rect.x as f32
                && top.1 < (mask.rect.x + mask.rect.width as i32) as f32
        );
        assert_eq!(
            super::rounded_span(mask, 26),
            Some((
                mask.rect.x as f32,
                (mask.rect.x + mask.rect.width as i32) as f32
            )),
            "straight rows retain the complete clip width"
        );
    }

    #[test]
    fn rounded_scene_clip_preserves_elliptical_per_corner_geometry() {
        let mask = ClipMask {
            rect: Rect {
                x: 10,
                y: 20,
                width: 40,
                height: 30,
            },
            radii: [
                CornerRadius { x: 12, y: 6 },
                CornerRadius { x: 4, y: 10 },
                CornerRadius { x: 8, y: 4 },
                CornerRadius { x: 2, y: 2 },
            ],
        };
        let top = super::rounded_span(mask, 20).expect("top row");
        let bottom = super::rounded_span(mask, 49).expect("bottom row");
        assert!(top.0 - mask.rect.x as f32 > 6.0);
        assert!((mask.rect.x + mask.rect.width as i32) as f32 - top.1 < 4.0);
        assert!(bottom.0 - (mask.rect.x as f32) < 2.0);
        assert!((mask.rect.x + mask.rect.width as i32) as f32 - bottom.1 > 4.0);
    }
}
