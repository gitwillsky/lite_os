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

// OWNER: 原生内存压力事件的唯一对象，不另存水位镜像；frame allocator 是实际水位 owner。
// 事件初始化有当前快照，随后只由 boot CPU 的 deferred observer 发布 level，避免迟到通知倒序。
static MEMORY_PRESSURE: spin::Once<alloc::sync::Arc<Event>> = spin::Once::new();

fn pressure_signals() -> (u32, u32) {
    use native_abi::system::{MEMORY_NORMAL, MEMORY_PRESSURE};
    if crate::memory::frame_allocator::under_pressure() {
        (MEMORY_NORMAL, MEMORY_PRESSURE)
    } else {
        (MEMORY_PRESSURE, MEMORY_NORMAL)
    }
}

/// 获取只读系统事件的共享对象；syscall 层负责资源授权及去掉 SIGNAL 权限。
///
/// # Errors
/// 对象分配失败返回 `Err(())`；初始化只发表准备好的 Arc，不在 Once 临界区分配或唤醒。
pub(crate) fn memory_pressure_event() -> Result<alloc::sync::Arc<Event>, ()> {
    if let Some(event) = MEMORY_PRESSURE.get() {
        return Ok(event.clone());
    }
    let event = alloc::sync::Arc::try_new(Event::new()).map_err(|_| ())?;
    let (clear, set) = pressure_signals();
    event.signals.update(clear, set);
    let event = MEMORY_PRESSURE.call_once(|| event).clone();
    // 封闭准备快照 → 发表之间的水位变更；唯一发布者稍后按实际水位校准，不永久遗失边沿。
    crate::memory::frame_allocator::request_pressure_notification();
    Ok(event)
}

/// main 注册给 frame allocator 的观察者；只在 boot CPU deferred 安全点调用。
pub(crate) fn notify_memory_pressure() {
    if let Some(event) = MEMORY_PRESSURE.get() {
        let (clear, set) = pressure_signals();
        event.signals.update(clear, set);
    }
}
