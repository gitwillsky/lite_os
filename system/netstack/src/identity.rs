//! 本次运行内不复用的对象标识：路由记录与即时回复的稳定排序键。

use core::sync::atomic::{AtomicU64, Ordering};

// OWNER: 唯一单调计数；缺失时同一前缀下的路由与回复队列没有稳定的插入次序。
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
