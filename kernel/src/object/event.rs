//! Event：只承载 signal 的同步对象。

use super::signals::SignalState;

/// 用户可置位 `SIGNALED` 与用户 signal 的事件。
pub(crate) struct Event {
    koid: u64,
    signals: SignalState,
}

impl Event {
    /// 构造未置位的 event。
    pub(crate) fn new() -> Self {
        Self {
            koid: super::next_koid(),
            signals: SignalState::new(0),
        }
    }
}

impl super::KernelObject for Event {
    fn object_type(&self) -> native_abi::ObjectType {
        native_abi::ObjectType::Event
    }
    fn koid(&self) -> u64 {
        self.koid
    }
    fn default_rights(&self) -> u32 {
        super::WAITABLE_RIGHTS | native_abi::rights::SIGNAL
    }
    fn user_signal_mask(&self) -> u32 {
        native_abi::signals::USER_ALL | native_abi::signals::SIGNALED
    }
    fn signals(&self) -> Option<&SignalState> {
        Some(&self.signals)
    }
}
