//! 本次运行内不复用的对象标识：路由记录、原生端点与绑定/即时回复的稳定身份。

use core::sync::atomic::{AtomicU64, Ordering};

// OWNER: 唯一单调计数；缺失时路由/回复缺少稳定插入次序，原生 UDP 端点与实际绑定发布也可能复用旧身份。
static NEXT: AtomicU64 = AtomicU64::new(1);

/// 分配一个非零标识；仅用于排序与命名，不承担内存发布同步。
///
/// # Panics
///
/// 64 位空间耗尽。
pub(crate) fn next() -> u64 {
    NEXT.try_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
        current.checked_add(1)
    })
    .expect("object identity exhausted")
}
