//! Persistent CSS scroll offsets, scrollbar geometry and user-agent painting.

use super::Raster;

use super::{PhysicalRect, Renderer};

pub(super) const SCROLLBAR_WIDTH: f32 = 14.0;
const MINIMUM_THUMB_LENGTH: f32 = 18.0;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) struct ScrollOffset {
    /// Horizontal CSS-pixel offset from the scroll origin.
    pub(super) x: f32,
    /// Vertical CSS-pixel offset from the scroll origin.
    pub(super) y: f32,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct LogicalRect {
    /// Left edge in logical CSS pixels.
    pub(super) x: f32,
    /// Top edge in logical CSS pixels.
    pub(super) y: f32,
    /// Non-negative logical width.
    pub(super) width: f32,
    /// Non-negative logical height.
    pub(super) height: f32,
}

impl LogicalRect {
    pub(super) fn contains(self, x: f32, y: f32) -> bool {
        x >= self.x && y >= self.y && x < self.x + self.width && y < self.y + self.height
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) struct ScrollRegion {
    /// Stable React host-instance identity.
    pub(super) node_id: u64,
    /// Presented, ancestor-clipped scroll port used for wheel hit testing.
    pub(super) port: LogicalRect,
    /// Furthest valid offset on each axis.
    pub(super) maximum: ScrollOffset,
    /// Whether the horizontal axis accepts default scrolling.
    pub(super) scroll_x: bool,
    /// Whether the vertical axis accepts default scrolling.
    pub(super) scroll_y: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Axis {
    /// Horizontal scroll axis.
    Horizontal,
    /// Vertical scroll axis.
    Vertical,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Scrollbar {
    /// Scroll-container identity controlled by this scrollbar.
    pub(super) node_id: u64,
    /// Axis controlled by this scrollbar.
    pub(super) axis: Axis,
    /// Complete clickable track.
    pub(super) track: LogicalRect,
    /// Proportional draggable thumb.
    pub(super) thumb: LogicalRect,
    /// Furthest valid offset on this axis.
    pub(super) maximum: f32,
    /// Scroll-port extent used for one-page track clicks.
    pub(super) viewport: f32,
}

#[derive(Clone, Copy)]
pub(super) struct ScrollDrag {
    /// Scroll-container identity retained for the captured pointer sequence.
    pub(super) node_id: u64,
    axis: Axis,
    pointer_start: f32,
    offset_start: f32,
    maximum: f32,
    thumb_travel: f32,
}

impl Renderer {
    /// Applies one CSS-pixel wheel delta to the deepest eligible scroll container.
    ///
    /// Unconsumed delta chains to containing scroll ports, matching browser
    /// nested-scroll behavior at the start and end boundaries.
    ///
    /// `x`/`y` select the target in logical pixels; `delta_x`/`delta_y` are
    /// signed logical-pixel movement. Returns whether any offset changed.
    pub fn scroll_wheel(&mut self, x: i32, y: i32, delta_x: i32, delta_y: i32) -> bool {
        let (x, y) = (x as f32, y as f32);
        let mut remaining = ScrollOffset {
            x: delta_x as f32,
            y: delta_y as f32,
        };
        let mut changed = false;
        for region in self
            .scroll_regions
            .iter()
            .rev()
            .copied()
            .filter(|region| region.port.contains(x, y))
        {
            let offset = self.scroll_offsets.entry(region.node_id).or_default();
            if region.scroll_x && remaining.x != 0.0 {
                let before = offset.x;
                let (next, remainder) = consume_delta(before, region.maximum.x, remaining.x);
                offset.x = next;
                remaining.x = remainder;
                changed |= before != next;
            }
            if region.scroll_y && remaining.y != 0.0 {
                let before = offset.y;
                let (next, remainder) = consume_delta(before, region.maximum.y, remaining.y);
                offset.y = next;
                remaining.y = remainder;
                changed |= before != next;
            }
            if remaining.x == 0.0 && remaining.y == 0.0 {
                break;
            }
        }
        changed
    }

    /// Handles a pointer press on native scrollbar chrome.
    ///
    /// Returns `(consumed, changed)`: thumb presses begin a native drag, while
    /// track presses page by one viewport and may immediately change pixels.
    /// `x` and `y` are surface-local logical pointer coordinates.
    pub fn scrollbar_pointer_down(&mut self, x: i32, y: i32) -> (bool, bool) {
        let (x, y) = (x as f32, y as f32);
        let Some(bar) = self.scrollbars.iter().rev().copied().find(|bar| {
            bar.track.contains(x, y)
                && self
                    .scroll_regions
                    .iter()
                    .rev()
                    .find(|region| region.node_id == bar.node_id)
                    .is_some_and(|region| region.port.contains(x, y))
        }) else {
            return (false, false);
        };
        let pointer = axis_value(bar.axis, x, y);
        if bar.thumb.contains(x, y) {
            let thumb_length = axis_length(bar.axis, bar.thumb);
            self.scroll_drag = Some(ScrollDrag {
                node_id: bar.node_id,
                axis: bar.axis,
                pointer_start: pointer,
                offset_start: self.axis_offset(bar.node_id, bar.axis),
                maximum: bar.maximum,
                thumb_travel: (axis_length(bar.axis, bar.track) - thumb_length).max(0.0),
            });
            return (true, false);
        }

        let thumb_start = axis_start(bar.axis, bar.thumb);
        let delta = if pointer < thumb_start {
            -bar.viewport
        } else {
            bar.viewport
        };
        (
            true,
            self.change_axis(bar.node_id, bar.axis, delta, bar.maximum),
        )
    }

    /// Reports whether the latest user-agent scrollbar chrome covers a point.
    ///
    /// `x` and `y` are surface-local logical coordinates. Returns true for a
    /// track, thumb, or two-axis corner owned by native scrollbar input.
    pub fn scrollbar_at(&self, x: i32, y: i32) -> bool {
        let (x, y) = (x as f32, y as f32);
        if self.scrollbars.iter().any(|bar| {
            bar.track.contains(x, y)
                && self
                    .scroll_regions
                    .iter()
                    .find(|region| region.node_id == bar.node_id)
                    .is_some_and(|region| region.port.contains(x, y))
        }) {
            return true;
        }
        self.scrollbars.iter().any(|horizontal| {
            horizontal.axis == Axis::Horizontal
                && self.scrollbars.iter().any(|vertical| {
                    vertical.node_id == horizontal.node_id
                        && vertical.axis == Axis::Vertical
                        && (LogicalRect {
                            x: vertical.track.x,
                            y: horizontal.track.y,
                            width: vertical.track.width,
                            height: horizontal.track.height,
                        })
                        .contains(x, y)
                })
        })
    }

    /// Updates an active native scrollbar drag.
    ///
    /// `x` and `y` are current logical pointer coordinates. Returns whether
    /// projecting the captured drag changed its scroll offset.
    pub fn scrollbar_pointer_move(&mut self, x: i32, y: i32) -> bool {
        let Some(drag) = self.scroll_drag else {
            return false;
        };
        if drag.maximum <= 0.0 || drag.thumb_travel <= 0.0 {
            return false;
        }
        let pointer = axis_value(drag.axis, x as f32, y as f32);
        let requested =
            drag.offset_start + (pointer - drag.pointer_start) * drag.maximum / drag.thumb_travel;
        self.set_axis(drag.node_id, drag.axis, requested, drag.maximum)
    }

    /// Ends a native scrollbar drag and reports whether it consumed the pointer sequence.
    pub fn scrollbar_pointer_up(&mut self) -> bool {
        self.scroll_drag.take().is_some()
    }

    fn axis_offset(&self, node_id: u64, axis: Axis) -> f32 {
        let offset = self
            .scroll_offsets
            .get(&node_id)
            .copied()
            .unwrap_or_default();
        match axis {
            Axis::Horizontal => offset.x,
            Axis::Vertical => offset.y,
        }
    }

    fn change_axis(&mut self, node_id: u64, axis: Axis, delta: f32, maximum: f32) -> bool {
        let requested = self.axis_offset(node_id, axis) + delta;
        self.set_axis(node_id, axis, requested, maximum)
    }

    fn set_axis(&mut self, node_id: u64, axis: Axis, requested: f32, maximum: f32) -> bool {
        let offset = self.scroll_offsets.entry(node_id).or_default();
        let target = requested.clamp(0.0, maximum);
        let current = match axis {
            Axis::Horizontal => &mut offset.x,
            Axis::Vertical => &mut offset.y,
        };
        if *current == target {
            return false;
        }
        *current = target;
        true
    }
}

/// Consumes as much of one wheel delta as this scroll range permits.
pub(super) fn consume_delta(offset: f32, maximum: f32, delta: f32) -> (f32, f32) {
    let next = (offset + delta).clamp(0.0, maximum);
    (next, delta - (next - offset))
}

/// Returns a proportional overlay scrollbar for one scrollable axis.
pub(super) fn scrollbar(
    node_id: u64,
    axis: Axis,
    port: LogicalRect,
    maximum: f32,
    offset: f32,
    other_axis_visible: bool,
) -> Scrollbar {
    let (track, viewport) = match axis {
        Axis::Horizontal => (
            LogicalRect {
                x: port.x,
                y: port.y + (port.height - SCROLLBAR_WIDTH).max(0.0),
                width: (port.width
                    - if other_axis_visible {
                        SCROLLBAR_WIDTH
                    } else {
                        0.0
                    })
                .max(0.0),
                height: SCROLLBAR_WIDTH.min(port.height),
            },
            port.width,
        ),
        Axis::Vertical => (
            LogicalRect {
                x: port.x + (port.width - SCROLLBAR_WIDTH).max(0.0),
                y: port.y,
                width: SCROLLBAR_WIDTH.min(port.width),
                height: (port.height
                    - if other_axis_visible {
                        SCROLLBAR_WIDTH
                    } else {
                        0.0
                    })
                .max(0.0),
            },
            port.height,
        ),
    };
    let track_length = match axis {
        Axis::Horizontal => track.width,
        Axis::Vertical => track.height,
    };
    let content_length = viewport + maximum;
    let thumb_length = if maximum <= 0.0 {
        track_length
    } else {
        (track_length * viewport / content_length)
            .clamp(MINIMUM_THUMB_LENGTH.min(track_length), track_length)
    };
    let thumb_travel = track_length - thumb_length;
    let thumb_offset = if maximum <= 0.0 {
        0.0
    } else {
        thumb_travel * offset.clamp(0.0, maximum) / maximum
    };
    let thumb = match axis {
        Axis::Horizontal => LogicalRect {
            x: track.x + thumb_offset,
            y: track.y,
            width: thumb_length,
            height: track.height,
        },
        Axis::Vertical => LogicalRect {
            x: track.x,
            y: track.y + thumb_offset,
            width: track.width,
            height: thumb_length,
        },
    };
    Scrollbar {
        node_id,
        axis,
        track,
        thumb,
        maximum,
        viewport,
    }
}

/// Paints one neutral user-agent scrollbar above the scrolled descendants.
pub(super) fn paint_scrollbar<R: Raster>(
    pixels: &mut R,
    scrollbar: Scrollbar,
    clip: Option<PhysicalRect>,
) {
    let track = physical(scrollbar.track, pixels, clip);
    fill(pixels, track, 0xffd4_d0c8);
    let thumb = physical(scrollbar.thumb, pixels, clip);
    fill(pixels, thumb, 0xffd4_d0c8);
    if thumb.x2 <= thumb.x1 || thumb.y2 <= thumb.y1 {
        return;
    }
    let light = 0xffff_ffff;
    let dark = 0xff40_4040;
    fill(
        pixels,
        PhysicalRect {
            x1: thumb.x1,
            y1: thumb.y1,
            x2: thumb.x2,
            y2: (thumb.y1 + 1).min(thumb.y2),
        },
        light,
    );
    fill(
        pixels,
        PhysicalRect {
            x1: thumb.x1,
            y1: thumb.y1,
            x2: (thumb.x1 + 1).min(thumb.x2),
            y2: thumb.y2,
        },
        light,
    );
    fill(
        pixels,
        PhysicalRect {
            x1: thumb.x1,
            y1: thumb.y2.saturating_sub(1),
            x2: thumb.x2,
            y2: thumb.y2,
        },
        dark,
    );
    fill(
        pixels,
        PhysicalRect {
            x1: thumb.x2.saturating_sub(1),
            y1: thumb.y1,
            x2: thumb.x2,
            y2: thumb.y2,
        },
        dark,
    );
}

/// Paints the square where simultaneous horizontal and vertical tracks meet.
pub(super) fn paint_scrollbar_corner<R: Raster>(
    pixels: &mut R,
    port: LogicalRect,
    clip: Option<PhysicalRect>,
) {
    let corner = LogicalRect {
        x: port.x + (port.width - SCROLLBAR_WIDTH).max(0.0),
        y: port.y + (port.height - SCROLLBAR_WIDTH).max(0.0),
        width: SCROLLBAR_WIDTH.min(port.width),
        height: SCROLLBAR_WIDTH.min(port.height),
    };
    let corner = physical(corner, pixels, clip);
    fill(pixels, corner, 0xffd4_d0c8);
}

fn physical<R: Raster>(
    rect: LogicalRect,
    pixels: &R,
    clip: Option<PhysicalRect>,
) -> PhysicalRect {
    let bounds = PhysicalRect::new(
        rect.x,
        rect.y,
        rect.width,
        rect.height,
        pixels.width(),
        pixels.height(),
    );
    clip.map_or(bounds, |clip| bounds.intersect(clip))
}

fn fill<R: Raster>(pixels: &mut R, rect: PhysicalRect, color: u32) {
    if rect.x2 <= rect.x1 || rect.y2 <= rect.y1 {
        return;
    }
    for y in rect.y1..rect.y2 {
        pixels.row_mut(y)[rect.x1..rect.x2].fill(color);
    }
}

fn axis_value(axis: Axis, x: f32, y: f32) -> f32 {
    match axis {
        Axis::Horizontal => x,
        Axis::Vertical => y,
    }
}

fn axis_start(axis: Axis, rect: LogicalRect) -> f32 {
    match axis {
        Axis::Horizontal => rect.x,
        Axis::Vertical => rect.y,
    }
}

fn axis_length(axis: Axis, rect: LogicalRect) -> f32 {
    match axis {
        Axis::Horizontal => rect.width,
        Axis::Vertical => rect.height,
    }
}

#[cfg(test)]
mod tests {
    use super::{Axis, LogicalRect, consume_delta, scrollbar};
    use taffy::prelude::{AvailableSpace, Dimension, Display, Size, Style, TaffyTree};
    use taffy::{Overflow, Point};

    #[test]
    fn content_that_fits_consumes_no_wheel_delta() {
        assert_eq!(consume_delta(0.0, 0.0, 48.0), (0.0, 48.0));
    }

    #[test]
    fn taffy_content_extent_produces_the_exact_scroll_boundary() {
        let mut tree = TaffyTree::<()>::new();
        let content = tree
            .new_leaf(Style {
                size: Size {
                    width: Dimension::length(100.0),
                    height: Dimension::length(300.0),
                },
                ..Style::default()
            })
            .expect("content node");
        let port = tree
            .new_with_children(
                Style {
                    display: Display::Block,
                    size: Size {
                        width: Dimension::length(100.0),
                        height: Dimension::length(100.0),
                    },
                    overflow: Point {
                        x: Overflow::Hidden,
                        y: Overflow::Scroll,
                    },
                    ..Style::default()
                },
                &[content],
            )
            .expect("scroll port");
        tree.compute_layout(
            port,
            Size {
                width: AvailableSpace::Definite(100.0),
                height: AvailableSpace::Definite(100.0),
            },
        )
        .expect("scroll layout");
        let layout = tree.layout(port).expect("scroll port layout");

        assert_eq!(layout.content_size.height, 300.0);
        assert_eq!(layout.content_box_height(), 100.0);
        assert_eq!(
            layout.content_size.height - layout.content_box_height(),
            200.0
        );
    }

    #[test]
    fn boundary_remainder_can_chain_to_an_ancestor() {
        assert_eq!(consume_delta(90.0, 100.0, 48.0), (100.0, 38.0));
        assert_eq!(consume_delta(40.0, 200.0, 38.0), (78.0, 0.0));
    }

    #[test]
    fn thumb_maps_the_complete_vertical_scroll_range() {
        let port = LogicalRect {
            x: 0.0,
            y: 0.0,
            width: 200.0,
            height: 300.0,
        };
        let start = scrollbar(1, Axis::Vertical, port, 300.0, 0.0, false);
        let end = scrollbar(1, Axis::Vertical, port, 300.0, 300.0, false);

        assert_eq!(start.thumb.height, 150.0);
        assert_eq!(start.thumb.y, 0.0);
        assert_eq!(end.thumb.y, 150.0);
    }

    #[test]
    fn non_overflowing_scroll_axis_uses_the_complete_track() {
        let bar = scrollbar(
            1,
            Axis::Horizontal,
            LogicalRect {
                x: 10.0,
                y: 20.0,
                width: 240.0,
                height: 100.0,
            },
            0.0,
            0.0,
            false,
        );

        assert_eq!(bar.thumb.x, bar.track.x);
        assert_eq!(bar.thumb.width, bar.track.width);
    }
}
