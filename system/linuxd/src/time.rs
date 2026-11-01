//! linuxd 的时间源：内核单调时钟与 UTC。

use crate::sync::Once;

use crate::system_info;

const NSEC_PER_SEC: u64 = 1_000_000_000;

/// 单调时钟纳秒。
pub(crate) fn get_time_ns() -> u64 {
    native_runtime::system::monotonic() as u64
}

/// 单调时钟微秒。
pub(crate) fn get_time_us() -> u64 {
    get_time_ns() / 1000
}

/// Unix epoch 纳秒（`CLOCK_REALTIME`）。
pub(crate) fn get_realtime_ns() -> u64 {
    native_runtime::system::utc() as u64
}

/// 设置 `CLOCK_REALTIME`。
///
/// # Errors
///
/// 时间早于本次启动（内核拒绝负偏移）返回 `Err(())`。
pub(crate) fn set_realtime_ns(realtime_ns: u64) -> Result<(), ()> {
    let utc = i64::try_from(realtime_ns).map_err(|_| ())?;
    native_runtime::system::set_utc(crate::handles::resource(), utc).map_err(|_| ())
}

/// 本次启动时刻的 Unix epoch 秒数。
pub(crate) fn boot_epoch_seconds() -> u64 {
    system_info().boot_utc_ns as u64 / NSEC_PER_SEC
}

/// 把绝对 realtime 时刻换算为单调时钟 deadline。
///
/// 按调用时刻的 realtime/monotonic 偏移换算：之后的墙钟跳变不移动已安排的 deadline，
/// 与内核定时器只使用单调 deadline 的语义一致（`clock_settime` 后由定时器所有者重排）。
pub(crate) fn realtime_deadline_to_monotonic_ns(realtime_ns: u64) -> u64 {
    let now_monotonic = get_time_ns();
    let now_realtime = get_realtime_ns();
    realtime_ns
        .saturating_sub(now_realtime)
        .saturating_add(now_monotonic)
}

/// 单调时钟的最小可观察粒度。
pub(crate) fn monotonic_resolution_ns() -> u64 {
    system_info().clock_resolution_ns
}

/// 调度器基础时间片（`sched_rr_get_interval`）。
pub(crate) fn scheduler_quantum_ns() -> u64 {
    // OWNER: 内核调度参数在启动后不变，缓存一次避免每次查询系统信息。
    static QUANTUM: Once<u64> = Once::new();
    *QUANTUM.call_once(|| system_info().scheduler_quantum_ns)
}
