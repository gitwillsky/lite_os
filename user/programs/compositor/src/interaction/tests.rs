use wayland_wire::wire::ObjectId;

use super::{Edges, Grab, Kind, MIN_HEIGHT, MIN_WIDTH};
use crate::wayland::ClientId;

const SCREEN: (i32, i32) = (1280, 800);

fn grab(kind: Kind) -> Grab {
    Grab {
        client: ClientId(1),
        surface: ObjectId::new(5).unwrap(),
        kind,
        pointer: (200, 200),
        start: (100, 100, 400, 300),
    }
}

fn edges(left: bool, right: bool, top: bool, bottom: bool) -> Edges {
    Edges {
        left,
        right,
        top,
        bottom,
    }
}

#[test]
fn xdg_edges_decode_and_reject_impossible_combinations() {
    assert_eq!(
        Edges::from_xdg(10),
        Some(edges(false, true, false, true)),
        "bottom right"
    );
    assert_eq!(
        Edges::from_xdg(5),
        Some(edges(true, false, true, false)),
        "top left"
    );
    assert_eq!(Edges::from_xdg(2), Some(edges(false, false, false, true)));
    for invalid in [0, 3, 12, 15, 16, 0x1000] {
        assert_eq!(Edges::from_xdg(invalid), None, "{invalid}");
    }
}

#[test]
fn moving_follows_the_pointer_delta() {
    let grab = grab(Kind::Move);
    assert_eq!(grab.moved_to((260, 230), SCREEN, 1), (160, 130));
    assert_eq!(
        grab.moved_to((200, 200), SCREEN, 1),
        (100, 100),
        "no movement"
    );
}

#[test]
fn moving_keeps_the_title_bar_reachable() {
    let grab = grab(Kind::Move);
    // 不能把标题栏拖到屏幕上方。
    assert_eq!(grab.moved_to((200, -500), SCREEN, 1).1, 0);
    // 不能整个拖出下边：至少留一个标题栏高度。
    assert_eq!(grab.moved_to((200, 5000), SCREEN, 1).1, 800 - 19);
    // 左右各至少留 64 像素。
    assert_eq!(grab.moved_to((-5000, 200), SCREEN, 1).0, 64 - 400);
    assert_eq!(grab.moved_to((5000, 200), SCREEN, 1).0, 1280 - 64);
    // 2x 下可见量也按物理像素翻倍。
    assert_eq!(grab.moved_to((200, 5000), SCREEN, 2).1, 800 - 38);
}

#[test]
fn resizing_the_bottom_right_corner_grows_the_window_in_place() {
    let grab = grab(Kind::Resize(edges(false, true, false, true)));
    assert_eq!(grab.resized_to((250, 260), 1), (100, 100, 450, 360));
}

#[test]
fn resizing_left_and_top_edges_keeps_the_opposite_edges_anchored() {
    let grab = grab(Kind::Resize(edges(true, false, true, false)));
    // 向左上拖 30、20：窗口变大，右下角不动（x+width=500, y+height=400）。
    let (x, y, width, height) = grab.resized_to((170, 180), 1);
    assert_eq!((width, height), (430, 320));
    assert_eq!((x + width, y + height), (500, 400));
    // 向右下拖：窗口变小，右下角仍然不动。
    let (x, y, width, height) = grab.resized_to((260, 240), 1);
    assert_eq!((width, height), (340, 260));
    assert_eq!((x + width, y + height), (500, 400));
}

#[test]
fn a_single_edge_only_changes_one_dimension() {
    let right = grab(Kind::Resize(edges(false, true, false, false)));
    assert_eq!(right.resized_to((300, 500), 1), (100, 100, 500, 300));
    let top = grab(Kind::Resize(edges(false, false, true, false)));
    assert_eq!(top.resized_to((900, 150), 1), (100, 50, 400, 350));
}

#[test]
fn resizing_stops_at_the_minimum_size_and_the_anchor_holds() {
    let grab = grab(Kind::Resize(edges(true, false, true, false)));
    let (x, y, width, height) = grab.resized_to((5000, 5000), 1);
    assert_eq!((width, height), (MIN_WIDTH, MIN_HEIGHT));
    assert_eq!((x + width, y + height), (500, 400));
    let scaled = grab.resized_to((5000, 5000), 2);
    assert_eq!((scaled.2, scaled.3), (MIN_WIDTH * 2, MIN_HEIGHT * 2));
}

#[test]
fn a_move_grab_never_resizes() {
    assert_eq!(
        grab(Kind::Move).resized_to((900, 900), 1),
        (100, 100, 400, 300)
    );
}
