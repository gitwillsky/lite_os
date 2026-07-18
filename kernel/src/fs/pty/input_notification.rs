/// PTY master drain 一批 raw input 后的锁外 consequence。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct PtyInputActions {
    pub(super) notify_slave: bool,
    pub(super) signals: u64,
}

/// @description 决定 PTY slave readiness 与 foreground signal 路由。
/// @param cooked_ready 当前批次是否已发布可读 cooked input/EOF。
/// @param raw_backlog 固定 256-byte line-discipline 批次后是否仍有 raw input。
/// @param signals line discipline 生成的 Linux signal bitset。
/// @return raw/cooked 任一可前进就通知 slave；signals 原样交给 task composition callback。
pub(super) const fn pty_input_actions(
    cooked_ready: bool,
    raw_backlog: bool,
    signals: u64,
) -> PtyInputActions {
    PtyInputActions {
        notify_slave: cooked_ready || raw_backlog,
        signals,
    }
}
