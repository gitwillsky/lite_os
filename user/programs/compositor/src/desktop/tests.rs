use wayland_wire::wire::ObjectId;

use super::{Mode, Stack};
use crate::wayland::ClientId;

fn surface(value: u32) -> ObjectId {
    ObjectId::new(value).unwrap()
}

const SCREEN: (i32, i32) = (1280, 800);
const FULL: (i32, i32, i32, i32) = (0, 0, 1280, 800);

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

fn two_windows() -> Stack {
    let mut stack = Stack::default();
    stack.sync(
        &[
            (ClientId(1), surface(5), 300, 200),
            (ClientId(2), surface(5), 300, 200),
        ],
        SCREEN,
        1,
    );
    stack
}

#[test]
fn maximize_saves_the_geometry_and_restore_brings_it_back() {
    let mut stack = two_windows();
    let (client, window) = (ClientId(2), surface(5));
    assert_eq!(
        stack.set_mode(client, window, Mode::Maximized, FULL, FULL),
        Some(SCREEN)
    );
    let placed = stack.placement(client, window).unwrap();
    assert_eq!(
        (placed.x, placed.y, placed.width, placed.height, placed.mode),
        (0, 0, 1280, 800, Mode::Maximized)
    );
    assert_eq!(placed.restore, Some((59, 59, 300, 200)));
    assert_eq!(
        stack.set_mode(client, window, Mode::Maximized, FULL, FULL),
        None,
        "no change"
    );
    // 最大化 → 全屏不覆盖已保存的还原几何。
    stack.set_mode(client, window, Mode::Fullscreen, FULL, FULL);
    assert_eq!(
        stack.placement(client, window).unwrap().restore,
        Some((59, 59, 300, 200))
    );
    assert_eq!(
        stack.set_mode(client, window, Mode::Normal, FULL, FULL),
        Some((300, 200))
    );
    let placed = stack.placement(client, window).unwrap();
    assert_eq!(
        (placed.x, placed.y, placed.mode, placed.restore),
        (59, 59, Mode::Normal, None)
    );
}

#[test]
fn maximized_windows_cannot_be_moved_and_normal_ones_can() {
    let mut stack = two_windows();
    assert!(stack.move_to(ClientId(2), surface(5), 100, 120));
    assert!(
        !stack.move_to(ClientId(2), surface(5), 100, 120),
        "same position is not a change"
    );
    stack.set_mode(ClientId(2), surface(5), Mode::Maximized, FULL, FULL);
    assert!(!stack.move_to(ClientId(2), surface(5), 5, 5));
    assert!(
        !stack.move_to(ClientId(9), surface(5), 5, 5),
        "unknown window"
    );
}

#[test]
fn minimized_windows_are_invisible_to_hit_testing_and_activation() {
    let mut stack = two_windows();
    assert!(stack.minimize(ClientId(2), surface(5)));
    assert!(
        !stack.minimize(ClientId(2), surface(5)),
        "already minimized"
    );
    assert_eq!(stack.active(), Some((ClientId(1), surface(5))));
    assert_eq!(
        stack.window_at(100, 100).unwrap().client,
        ClientId(1),
        "the window below is hit instead"
    );
    // 全部最小化后没有活动窗口。
    stack.minimize(ClientId(1), surface(5));
    assert_eq!(stack.active(), None);
    assert!(stack.window_at(100, 100).is_none());
    // 激活会恢复最小化并提到最上层，即使它本来就在栈顶。
    assert!(stack.raise(ClientId(2), surface(5)));
    assert_eq!(stack.active(), Some((ClientId(2), surface(5))));
    assert!(!stack.placement(ClientId(2), surface(5)).unwrap().minimized);
}

#[test]
fn mode_and_minimized_state_survive_sync() {
    let mut stack = two_windows();
    stack.set_mode(ClientId(1), surface(5), Mode::Maximized, FULL, FULL);
    stack.minimize(ClientId(1), surface(5));
    stack.sync(
        &[
            (ClientId(1), surface(5), 1280, 800),
            (ClientId(2), surface(5), 300, 200),
        ],
        SCREEN,
        1,
    );
    let placed = stack.placement(ClientId(1), surface(5)).unwrap();
    assert!(placed.minimized && placed.mode == Mode::Maximized);
    assert!(!stack.sync(
        &[
            (ClientId(1), surface(5), 1280, 800),
            (ClientId(2), surface(5), 300, 200)
        ],
        SCREEN,
        1
    ));
}

#[test]
fn alt_tab_rotates_through_least_recently_used_and_alt_esc_lowers() {
    let mut stack = Stack::default();
    assert_eq!(stack.cycle_target(), None);
    stack.sync(&[(ClientId(1), surface(5), 100, 100)], SCREEN, 1);
    assert_eq!(
        stack.cycle_target(),
        None,
        "one window has nothing to switch to"
    );
    stack.sync(
        &[
            (ClientId(1), surface(5), 100, 100),
            (ClientId(2), surface(5), 100, 100),
            (ClientId(3), surface(5), 100, 100),
        ],
        SCREEN,
        1,
    );
    // 栈底→栈顶：1, 2, 3；Alt+Tab 激活 1，再按激活 2，再按 3。
    let order: Vec<u64> = (0..3)
        .map(|_| {
            let (client, window) = stack.cycle_target().unwrap();
            stack.raise(client, window);
            client.0
        })
        .collect();
    assert_eq!(order, [1, 2, 3]);
    // Alt+Esc：活动窗口去最底层，下一个成为活动窗口。
    let (client, window) = stack.active().unwrap();
    assert!(stack.lower(client, window));
    assert_eq!(stack.windows()[0].client, client);
    assert_ne!(stack.active().unwrap().0, client);
    assert!(!stack.lower(client, window), "already at the bottom");
}

#[test]
fn maximize_fills_the_work_area_and_fullscreen_the_whole_output() {
    let mut stack = two_windows();
    let work = (0, 0, 1280, 772);
    assert_eq!(
        stack.set_mode(ClientId(2), surface(5), Mode::Maximized, work, FULL),
        Some((1280, 772))
    );
    assert_eq!(
        stack.set_mode(ClientId(2), surface(5), Mode::Fullscreen, work, FULL),
        Some((1280, 800))
    );
    let placed = stack.placement(ClientId(2), surface(5)).unwrap();
    assert_eq!(
        (placed.x, placed.y, placed.width, placed.height),
        (0, 0, 1280, 800)
    );
    assert_eq!(
        placed.restore,
        Some((59, 59, 300, 200)),
        "the original geometry survives both modes"
    );
}

#[test]
fn refit_follows_the_work_area_for_maximized_windows_only() {
    let mut stack = two_windows();
    stack.set_mode(ClientId(1), surface(5), Mode::Maximized, FULL, FULL);
    stack.set_mode(ClientId(2), surface(5), Mode::Fullscreen, FULL, FULL);
    assert!(stack.refit(FULL, FULL).is_empty(), "nothing changed");
    // 任务栏出现：工作区缩小，只有最大化的窗口被重新适配，全屏窗口不动。
    let work = (0, 0, 1280, 772);
    assert_eq!(
        stack.refit(work, FULL),
        [(ClientId(1), surface(5), (1280, 772))]
    );
    assert_eq!(
        stack.placement(ClientId(1), surface(5)).unwrap().height,
        772
    );
    assert_eq!(
        stack.placement(ClientId(2), surface(5)).unwrap().height,
        800
    );
    // 普通窗口永远不被 refit 触碰。
    let mut normal = two_windows();
    assert!(normal.refit(work, FULL).is_empty());
}
