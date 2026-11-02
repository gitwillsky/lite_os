//! 完成线程：驱动在中断处理中只确认设备并调用 [`raise`]，完成线程随后分派全部 completion 源，
//! 唤醒等待请求的服务线程。多次发布合并为一轮。

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};

use native_runtime::native_abi::DEADLINE_INFINITE;
use native_runtime::sync::Once;
use native_runtime::thread::Descriptor;

// OWNER: 有待分派的完成；发布者置位，完成线程在分派前清除。缺失时两次发布之间的完成可能被遗漏。
static PENDING: AtomicBool = AtomicBool::new(false);
// OWNER: 完成线程描述符；启动时发布一次。发布前的 raise 只置位，线程启动后第一轮即分派。
static WORKER: Once<Arc<Descriptor>> = Once::new();

/// 请求一轮完成分派。
pub(crate) fn raise() {
    PENDING.store(true, Ordering::Release);
    if let Some(worker) = WORKER.get() {
        worker.unpark();
    }
}

/// 启动完成线程。
///
/// # Panics
///
/// 线程创建失败：没有它任何驱动请求都不会完成。
pub(crate) fn start() {
    let thread = native_runtime::thread::spawn(b"driver-completion", || {
        WORKER.call_once(native_runtime::thread::current_arc);
        loop {
            if !PENDING.swap(false, Ordering::AcqRel) {
                let _ = native_runtime::thread::current().park(DEADLINE_INFINITE);
                continue;
            }
            if crate::io_completion::dispatch_io_completion_work() {
                PENDING.store(true, Ordering::Release);
            }
        }
    })
    .expect("driver completion thread");
    drop(thread);
}
