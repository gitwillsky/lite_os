use wayland_wire::wire::ObjectId;

use super::Stack;
use crate::wayland::ClientId;

fn surface(value: u32) -> ObjectId {
    ObjectId::new(value).unwrap()
}

const SCREEN: (i32, i32) = (1280, 800);

#[test]
fn new_windows_cascade_by_a_title_bar_and_land_on_top() {
    let mut stack = Stack::default();
    let changed = stack.sync(&[(ClientId(1), surface(5), 300, 200)], SCREEN, 1);
    assert!(changed);
    assert_eq!((stack.windows()[0].x, stack.windows()[0].y), (40, 40));
    stack.sync(
        &[
            (ClientId(1), surface(5), 300, 200),
            (ClientId(2), surface(5), 300, 200),
        ],
        SCREEN,
        1,
    );
    assert_eq!((stack.windows()[1].x, stack.windows()[1].y), (59, 59));
    assert_eq!(stack.windows()[1].client, ClientId(2));
}

#[test]
fn cascade_scales_with_the_output_and_wraps_before_leaving_the_screen() {
    let mut stack = Stack::default();
    stack.sync(&[(ClientId(1), surface(5), 300, 200)], SCREEN, 2);
    assert_eq!(stack.windows()[0].x, 80);
    let mut stack = Stack::default();
    let mut mapped = Vec::new();
    for client in 0..40 {
        mapped.push((ClientId(client), surface(5), 300, 200));
        stack.sync(&mapped, SCREEN, 1);
    }
    assert!(stack.windows().iter().all(|placed| placed.x + placed.width < SCREEN.0 && placed.y + placed.height < SCREEN.1));
    assert!(
        stack.windows().iter().any(|placed| placed.x == 40),
        "the cascade restarted"
    );
}

#[test]
fn sync_is_idempotent_and_reports_only_real_changes() {
    let mut stack = Stack::default();
    let mapped = [(ClientId(1), surface(5), 300, 200)];
    assert!(stack.sync(&mapped, SCREEN, 1));
    assert!(!stack.sync(&mapped, SCREEN, 1));
    assert!(
        stack.sync(&[(ClientId(1), surface(5), 320, 200)], SCREEN, 1),
        "resize is a change"
    );
    assert_eq!(stack.windows()[0].width, 320);
    assert_eq!(
        (stack.windows()[0].x, stack.windows()[0].y),
        (40, 40),
        "position is kept on resize"
    );
    assert!(stack.sync(&[], SCREEN, 1));
    assert!(stack.windows().is_empty());
}

#[test]
fn identity_is_client_and_surface_not_position_in_the_list() {
    let mut stack = Stack::default();
    stack.sync(
        &[
            (ClientId(1), surface(5), 100, 100),
            (ClientId(2), surface(5), 100, 100),
        ],
        SCREEN,
        1,
    );
    // 第一个连接断开：第二个窗口保持原位，不会被当成新窗口重新层叠。
    stack.sync(&[(ClientId(2), surface(5), 100, 100)], SCREEN, 1);
    assert_eq!(stack.windows().len(), 1);
    assert_eq!(
        (stack.windows()[0].client, stack.windows()[0].x),
        (ClientId(2), 59)
    );
}

#[test]
fn hit_testing_prefers_the_topmost_window() {
    let mut stack = Stack::default();
    stack.sync(
        &[
            (ClientId(1), surface(5), 300, 200),
            (ClientId(2), surface(5), 300, 200),
        ],
        SCREEN,
        1,
    );
    assert_eq!(stack.window_at(100, 100).unwrap().client, ClientId(2));
    assert_eq!(stack.window_at(45, 45).unwrap().client, ClientId(1));
    assert!(stack.window_at(5, 5).is_none());
    assert!(stack.window_at(359, 100).is_none());
    assert_eq!(stack.window_at(358, 258).unwrap().client, ClientId(2));
}

#[test]
fn the_top_of_the_stack_is_the_active_window_and_raise_changes_it() {
    let mut stack = Stack::default();
    assert_eq!(stack.active(), None);
    stack.sync(
        &[
            (ClientId(1), surface(5), 100, 100),
            (ClientId(2), surface(5), 100, 100),
        ],
        SCREEN,
        1,
    );
    assert_eq!(
        stack.active(),
        Some((ClientId(2), surface(5))),
        "a new window becomes active"
    );
    assert!(stack.raise(ClientId(1), surface(5)));
    assert_eq!(stack.active(), Some((ClientId(1), surface(5))));
    assert_eq!(
        stack.windows()[0].client,
        ClientId(2),
        "the other window keeps its relative place below"
    );
    assert!(!stack.raise(ClientId(1), surface(5)), "already on top");
    assert!(!stack.raise(ClientId(9), surface(5)), "unknown window");
    // 栈顶窗口消失后，下一个窗口自动成为活动窗口。
    stack.sync(&[(ClientId(2), surface(5), 100, 100)], SCREEN, 1);
    assert_eq!(stack.active(), Some((ClientId(2), surface(5))));
}
