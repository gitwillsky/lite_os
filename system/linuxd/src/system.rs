pub(crate) mod task_observer;

pub(crate) use crate::arch::user::SignalActionBytes;
pub(crate) use crate::platform::ResetKind;

/// 返回 immutable build facts 与启动默认 UTS 名字；live hostname/domainname 由 UTS owner 提供。
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

/// 返回标准 seccomp/siginfo 使用的 Linux AUDIT_ARCH architecture identity。
pub(crate) const fn audit_architecture() -> u32 {
    crate::arch::user::AUDIT_ARCH
}

/// 单调时钟计数器频率（RISC-V hwprobe `TIME_CSR_FREQ`）：由内核报告的时钟粒度换算。
pub(crate) fn time_counter_frequency() -> u64 {
    1_000_000_000 / crate::system_info().clock_resolution_ns.max(1)
}

/// 调用线程当前所在的 CPU（`getcpu`；只是提示）。
pub(crate) fn current_cpu_index() -> usize {
    crate::cpu::current_id().index()
}

/// 在线 CPU 位图。
pub(crate) fn online_cpu_mask() -> usize {
    crate::cpu::online().native_word()
}

/// 关机或冷重启整机。
///
/// # Errors
///
/// 内核拒绝请求。
pub(crate) fn reset(kind: ResetKind) -> Result<(), crate::platform::ResetError> {
    crate::platform::reset_system(kind)
}

/// 上一次启动的内核崩溃记录（`/proc/last_panic`）；上次正常结束时为 `None`。
pub(crate) fn previous_panic() -> Option<&'static [u8]> {
    // OWNER: 启动后读取一次的崩溃记录；内核只在启动时保存它，之后不变。
    static RECORD: crate::sync::Once<alloc::vec::Vec<u8>> = crate::sync::Once::new();
    let record = RECORD.call_once(|| {
        let mut buffer = alloc::vec![0u8; 64 * 1024];
        let length = native_runtime::system::get_info(
            native_runtime::native_abi::system::CRASH_LOG,
            &mut buffer,
        )
        .unwrap_or(0);
        buffer.truncate(length);
        buffer
    });
    (!record.is_empty()).then_some(record.as_slice())
}
