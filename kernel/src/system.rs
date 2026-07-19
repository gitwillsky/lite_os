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
        crate::arch::user::MACHINE_NAME,
        "(none)",
    ]
}

/// @description 投影所有 online CPU 共同成立的保守 Linux/riscv64 hwprobe value。
/// @param key Linux `RISCV_HWPROBE_KEY_*`。
/// @return 已知 key/value；未知 key 返回 None，由 syscall 编码 key=-1。
pub(crate) fn riscv_hwprobe_value(key: i64) -> Option<u64> {
    crate::arch::user::hardware_probe_value(key, crate::platform::timebase_frequency())
}

/// @description 判断当前编译期 architecture 是否定义 Linux `riscv_hwprobe` syscall。
/// @return RISC-V 后端为 true；其他架构为 false，dispatcher 必须返回 `ENOSYS`。
pub(crate) const fn supports_riscv_hwprobe() -> bool {
    crate::arch::user::SUPPORTS_RISCV_HWPROBE
}

/// @description 返回 calling CPU 对应的紧凑 Linux logical CPU index。
///
/// @return 按 platform CPU ID 升序排列的零基 CPU index。
/// @panics calling CPU 不属于已发布 topology 时 fail-stop。
pub(crate) fn current_cpu_index() -> usize {
    crate::cpu::current_id().index()
}

/// @description 投影 CpuTopology 的 logical online CPU mask，供 Linux userspace ABI 校验 selector。
///
/// @return bit N 表示 logical CPU N 已 online。
pub(crate) fn online_cpu_mask() -> usize {
    crate::cpu::online().native_word()
}

/// @description 通过唯一 platform reset seam 关闭或冷重启整个 SMP system。
///
/// @param kind 已由 syscall UAPI 层验证的 reset 类型。
/// @return firmware 异常返回时传播 typed platform error；成功通常不返回。
pub(crate) fn reset(kind: ResetKind) -> Result<(), crate::platform::ResetError> {
    let reset_type = match kind {
        ResetKind::Shutdown => 0,
        ResetKind::ColdReboot => 1,
    };
    crate::platform::reset_system(reset_type, 0)
}

/// @description 更新 Linux Ctrl-Alt-Delete 的 whole-system reset policy。
///
/// @param enabled true 表示未来 CAD input 直接重启，false 表示交由 PID 1 处理。
/// @return 无返回值；策略使用原子状态，避免未来 input IRQ 与 syscall 并发时丢失更新。
pub(crate) fn set_ctrl_alt_del(enabled: bool) {
    CTRL_ALT_DEL_ENABLED.store(enabled, Ordering::Release);
}
