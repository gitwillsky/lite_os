//! 未实现 syscall 的可观测性：每个编号第一次返回 ENOSYS 时记一行日志。

use core::sync::atomic::{AtomicU64, Ordering};

/// 单独追踪的编号上界；不小于它的编号共用最后一位，避免恶意编号刷屏。
const TRACKED_NUMBERS: usize = 512;
const WORDS: usize = TRACKED_NUMBERS / 64;

// OWNER: 本模块独占 `REPORTED`；每位表示该编号已记过日志。Relaxed 足够：位只用于去重，不发布任何数据。
// 缺失后果：同一编号每次调用都打印，热循环里的未实现 syscall 会淹没串口与 kmsg。
static REPORTED: [AtomicU64; WORDS] = [const { AtomicU64::new(0) }; WORDS];

/// 记录一次未实现 syscall；每个编号（≥512 的共用一个）整个运行期至多输出一行。
///
/// # Parameters
///
/// - `syscall_id`: 用户传入的 raw syscall 编号，名称由宿主按 ABI 表解析。
pub(super) fn report(syscall_id: usize) {
    let slot = syscall_id.min(TRACKED_NUMBERS - 1);
    let bit = 1u64 << (slot % 64);
    if REPORTED[slot / 64].fetch_or(bit, Ordering::Relaxed) & bit == 0 {
        crate::warn!("unimplemented syscall {syscall_id} returns ENOSYS");
    }
}
