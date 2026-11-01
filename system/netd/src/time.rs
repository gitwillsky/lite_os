//! 单调时钟。

/// 单调时钟（微秒）。
pub(crate) fn get_time_us() -> u64 {
    native_runtime::system::monotonic() as u64 / 1000
}
