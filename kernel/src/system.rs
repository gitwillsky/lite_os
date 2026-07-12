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

/// @description 投影所有 online hart 共同成立的保守 RISC-V hwprobe value。
/// @param key Linux `RISCV_HWPROBE_KEY_*`。
/// @return 已知 key/value；未知 key 返回 None，由 syscall 编码 key=-1。
pub(crate) fn riscv_hwprobe_value(key: i64) -> Option<u64> {
    const IMA: u64 = 1;
    const FD_AND_C: u64 = (1 << 0) | (1 << 1);
    const SV39_USER_ADDRESS_MAX: u64 = (1u64 << 38) - 1;
    match key {
        0..=2 => Some(0),
        3 => Some(IMA),
        4 => Some(FD_AND_C),
        5 | 6 | 9 | 11..=16 => Some(0),
        7 => Some(SV39_USER_ADDRESS_MAX),
        8 => Some(crate::arch::dtb::board_info().time_base_freq),
        10 => Some(4),
        _ => None,
    }
}

/// @description 投影 HartTopology 的 online mask，供 RISC-V userspace ABI 校验 CPU selector。
/// @return 当前已 online 的 DTB hart mask。
pub(crate) fn online_hart_mask() -> usize {
    crate::arch::hart::online_hart_mask()
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
