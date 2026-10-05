use core::sync::atomic::{AtomicBool, Ordering};

// OWNER: system module 唯一拥有 whole-system Ctrl-Alt-Delete policy。
static CTRL_ALT_DEL_ENABLED: AtomicBool = AtomicBool::new(true);

pub(crate) use crate::platform::ResetKind;

/// 返回唯一的 immutable system/build identity，供标准 utsname ABI 投影。
///
/// # Returns
///
/// 依次为 sysname、nodename、release、version、machine、domainname。
pub(crate) fn identity() -> [&'static str; 6] {
    [
        "LiteOS",
        "liteos",
        env!("CARGO_PKG_VERSION"),
        "#1 SMP PREEMPT",
        crate::arch::user::MACHINE_NAME,
        "(none)",
    ]
}

/// 通过编译期选中的 architecture decoder 过滤私有 Linux syscall number。
///
/// # Parameters
///
/// - `syscall_id`: raw Linux syscall number。
///
/// # Returns
///
/// 当前 backend 拥有该编号时返回原编号，否则返回 None。
#[inline(always)]
pub(crate) fn decode_architecture_syscall(syscall_id: usize) -> Option<usize> {
    crate::arch::user::decode_private_syscall(syscall_id)
}

/// 返回 platform monotonic counter 的固定频率。
///
/// # Returns
///
/// DTB/architecture platform owner 已验证的 Hz 值。
pub(crate) fn time_counter_frequency() -> u64 {
    crate::platform::timebase_frequency()
}

/// 返回 calling CPU 对应的紧凑 Linux logical CPU index。
///
/// # Returns
///
/// 按 platform CPU ID 升序排列的零基 CPU index。
///
/// # Panics
///
/// calling CPU 不属于已发布 topology 时 fail-stop。
pub(crate) fn current_cpu_index() -> usize {
    crate::cpu::current_id().index()
}

/// 投影 CpuTopology 的 logical online CPU mask，供 Linux userspace ABI 校验 selector。
///
/// # Returns
///
/// bit N 表示 logical CPU N 已 online。
pub(crate) fn online_cpu_mask() -> usize {
    crate::cpu::online().native_word()
}

/// 通过唯一 platform reset seam 关闭或冷重启整个 SMP system。
///
/// # Parameters
///
/// - `kind`: 已由 syscall UAPI 层验证的 reset 类型。
///
/// # Returns
///
/// firmware 异常返回时传播 typed platform error；成功通常不返回。
pub(crate) fn reset(kind: ResetKind) -> Result<(), crate::platform::ResetError> {
    crate::platform::reset_system(kind, crate::platform::ResetReason::Requested)
}

/// 更新 Linux Ctrl-Alt-Delete 的 whole-system reset polic。
///
/// # Parameters
///
/// - `enabled`: true 表示未来 CAD input 直接重启，false 表示交由 PID 1 处理。
///
/// # Returns
///
/// 无返回值；策略使用原子状态，避免未来 input IRQ 与 syscall 并发时丢失更新。
pub(crate) fn set_ctrl_alt_del(enabled: bool) {
    CTRL_ALT_DEL_ENABLED.store(enabled, Ordering::Release);
}
