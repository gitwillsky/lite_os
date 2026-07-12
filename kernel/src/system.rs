use core::sync::atomic::{AtomicBool, Ordering};

// OWNER: system module 唯一拥有 whole-system Ctrl-Alt-Delete policy。
static CTRL_ALT_DEL_ENABLED: AtomicBool = AtomicBool::new(true);

/// @description Linux reboot command 收敛后的 platform reset policy。
pub(crate) enum ResetKind {
    Shutdown,
    ColdReboot,
}

/// @description 返回唯一的 immutable system/build identity，供标准 utsname ABI 投影。
///
/// @return 依次为 sysname、nodename、release、version、machine、domainname。
pub(crate) fn identity() -> [&'static str; 6] {
    [
        "LiteOS",
        "liteos",
        env!("CARGO_PKG_VERSION"),
        "#1 SMP PREEMPT",
        "riscv64",
        "(none)",
    ]
}

/// @description 通过唯一 SBI SRST seam 关闭或冷重启整个 SMP system。
///
/// @param kind 已由 syscall UAPI 层验证的 reset 类型。
/// @return firmware 异常返回时传播 SBI error；成功通常不返回。
pub(crate) fn reset(kind: ResetKind) -> Result<(), isize> {
    let reset_type = match kind {
        ResetKind::Shutdown => 0,
        ResetKind::ColdReboot => 1,
    };
    crate::arch::sbi::system_reset(reset_type, 0)
}

/// @description 更新 Linux Ctrl-Alt-Delete 的 whole-system reset policy。
///
/// @param enabled true 表示未来 CAD input 直接重启，false 表示交由 PID 1 处理。
/// @return 无返回值；策略使用原子状态，避免未来 input IRQ 与 syscall 并发时丢失更新。
pub(crate) fn set_ctrl_alt_del(enabled: bool) {
    CTRL_ALT_DEL_ENABLED.store(enabled, Ordering::Release);
}
