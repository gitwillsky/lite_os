use super::{place, work_area};
use crate::wayland::{
    KeyboardInteractivity, Layer, LayerState,
    layer_edges::{ANCHOR_BOTTOM, ANCHOR_LEFT, ANCHOR_RIGHT, ANCHOR_TOP},
};

const SCREEN: (i32, i32) = (1280, 800);

fn state(anchor: u32, zone: i32, margin: [i32; 4]) -> LayerState {
    LayerState {
        layer: Layer::Top,
        anchor,
        width: 0,
        height: 0,
        margin,
        exclusive_zone: zone,
        exclusive_edge: 0,
        keyboard: KeyboardInteractivity::None,
    }
}

#[test]
fn a_bottom_bar_sits_on_the_bottom_edge_and_a_top_bar_on_the_top() {
    let bar = state(ANCHOR_BOTTOM | ANCHOR_LEFT | ANCHOR_RIGHT, 28, [0; 4]);
    assert_eq!(
        place(&bar, (1280, 28), (0, 0, SCREEN.0, SCREEN.1), 1),
        (0, 772)
    );
    let top = state(ANCHOR_TOP | ANCHOR_LEFT | ANCHOR_RIGHT, 0, [0; 4]);
    assert_eq!(
        place(&top, (1280, 28), (0, 0, SCREEN.0, SCREEN.1), 1),
        (0, 0)
    );
}

#[test]
fn margins_are_logical_pixels_scaled_to_the_output() {
    let bar = state(ANCHOR_BOTTOM | ANCHOR_LEFT, 0, [0, 0, 4, 10]);
    assert_eq!(
        place(&bar, (100, 20), (0, 0, SCREEN.0, SCREEN.1), 1),
        (10, 800 - 20 - 4)
    );
    assert_eq!(
        place(&bar, (200, 40), (0, 0, SCREEN.0, SCREEN.1), 2),
        (20, 800 - 40 - 8)
    );
    let right = state(ANCHOR_TOP | ANCHOR_RIGHT, 0, [3, 7, 0, 0]);
    assert_eq!(
        place(&right, (50, 50), (0, 0, SCREEN.0, SCREEN.1), 1),
        (1280 - 50 - 7, 3)
    );
}

#[test]
fn unanchored_directions_center_and_opposite_anchors_center_between_the_margins() {
    let centered = state(0, 0, [0; 4]);
    assert_eq!(
        place(&centered, (400, 300), (0, 0, SCREEN.0, SCREEN.1), 1),
        (440, 250)
    );
    let between = state(ANCHOR_LEFT | ANCHOR_RIGHT | ANCHOR_TOP, 0, [0, 100, 0, 20]);
    // 左右各留 20 与 100：可用宽度 1160，400 宽的 surface 在其中居中。
    assert_eq!(
        place(&between, (400, 50), (0, 0, SCREEN.0, SCREEN.1), 1),
        (20 + (1160 - 400) / 2, 0)
    );
}

#[test]
fn exclusive_zones_shrink_the_work_area_from_their_edge() {
    let bottom = state(ANCHOR_BOTTOM | ANCHOR_LEFT | ANCHOR_RIGHT, 28, [0; 4]);
    assert_eq!(work_area([&bottom], SCREEN, 1), (0, 0, 1280, 772));
    let top = state(ANCHOR_TOP | ANCHOR_LEFT | ANCHOR_RIGHT, 20, [0; 4]);
    assert_eq!(work_area([&bottom, &top], SCREEN, 1), (0, 20, 1280, 752));
    let left = state(ANCHOR_LEFT | ANCHOR_TOP | ANCHOR_BOTTOM, 40, [0; 4]);
    assert_eq!(work_area([&left], SCREEN, 1), (40, 0, 1240, 800));
    let right = state(ANCHOR_RIGHT | ANCHOR_TOP | ANCHOR_BOTTOM, 40, [0; 4]);
    assert_eq!(work_area([&left, &right], SCREEN, 1), (40, 0, 1200, 800));
}

#[test]
fn the_exclusive_zone_includes_the_margin_on_its_edge_and_scales() {
    let bar = state(ANCHOR_BOTTOM | ANCHOR_LEFT | ANCHOR_RIGHT, 28, [0, 0, 4, 0]);
    assert_eq!(work_area([&bar], SCREEN, 1), (0, 0, 1280, 800 - 32));
    assert_eq!(work_area([&bar], SCREEN, 2), (0, 0, 1280, 800 - 64));
}

#[test]
fn zero_negative_and_ambiguous_zones_reserve_nothing() {
    let none = state(ANCHOR_BOTTOM | ANCHOR_LEFT | ANCHOR_RIGHT, 0, [0; 4]);
    let extend = state(ANCHOR_BOTTOM | ANCHOR_LEFT | ANCHOR_RIGHT, -1, [0; 4]);
    let ambiguous = state(ANCHOR_LEFT | ANCHOR_RIGHT, 50, [0; 4]);
    assert_eq!(
        work_area([&none, &extend, &ambiguous], SCREEN, 1),
        (0, 0, 1280, 800)
    );
}

#[test]
fn an_explicit_exclusive_edge_overrides_the_anchor_rules() {
    let mut full = state(
        ANCHOR_TOP | ANCHOR_BOTTOM | ANCHOR_LEFT | ANCHOR_RIGHT,
        30,
        [0; 4],
    );
    full.exclusive_edge = ANCHOR_RIGHT;
    assert_eq!(work_area([&full], SCREEN, 1), (0, 0, 1250, 800));
}

#[test]
fn reserving_more_than_the_screen_clamps_at_zero() {
    let huge = state(ANCHOR_BOTTOM | ANCHOR_LEFT | ANCHOR_RIGHT, 5000, [0; 4]);
    assert_eq!(work_area([&huge], SCREEN, 1).3, 0);
}

#[test]
fn client_margin_extremes_cannot_overflow_or_expand_the_work_area() {
    let bottom = state(ANCHOR_BOTTOM, i32::MAX, [0, 0, i32::MAX, 0]);
    assert_eq!(work_area([&bottom], SCREEN, 2), (0, 0, 1280, 0));
    let top = state(ANCHOR_TOP, 20, [i32::MIN, 0, 0, 0]);
    assert_eq!(work_area([&top], SCREEN, 2), (0, 0, 1280, 800));
    let left = state(ANCHOR_LEFT, 0, [0, 0, 0, i32::MAX]);
    assert_eq!(
        place(&left, (10, 10), (0, 0, SCREEN.0, SCREEN.1), 2).0,
        i32::MAX
    );
}
