//! 时钟：UTC 用于 inode 时间戳，单调时钟用于写回周期。

/// 当前 UTC（纳秒）。
pub(crate) fn get_realtime_ns() -> u64 {
    u64::try_from(native_runtime::system::utc()).unwrap_or(0)
}

/// 单调时钟（纳秒）。
pub(crate) fn get_time_ns() -> u64 {
    native_runtime::system::monotonic() as u64
}
