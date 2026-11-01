//! 协议推进线程：收发帧、定时器、端点清理与就绪通知都在这里有界地推进。
//!
//! 1. 网卡活动与 socket 操作（新数据待发送、连接状态变化）请求一轮推进：向 port 投递一个 packet，
//!    尚未处理的请求合并为一个；
//! 2. 存在以太网卡时每 10ms 检查一次协议定时器（TCP 重传、ARP、UDP 出口）；
//! 3. 一轮推进报告仍有积压（接收预算用尽等）时立即再推进，直到积压清空。

use core::sync::atomic::{AtomicBool, Ordering};

use native_runtime::Port;
use native_runtime::native_abi::{self, DEADLINE_INFINITE, Status};
use native_runtime::sync::Once;

/// 协议定时器的轮询间隔（纳秒）。
const POLL_INTERVAL_NS: i64 = 10_000_000;

struct Worker {
    port: Port,
    // OWNER: 已投递而未被线程取走的推进请求；缺失时每次 socket 操作都在 port 上堆积一个 packet。
    requested: AtomicBool,
    // OWNER: 已接入以太网卡；只有它们的协议需要周期定时检查，纯回环由操作请求驱动。
    timed: AtomicBool,
}

// OWNER: 进程唯一推进线程的唤醒源；`start` 安装一次。
static WORKER: Once<Worker> = Once::new();

/// 创建推进线程。
///
/// # Errors
///
/// port 或线程创建失败。
pub(crate) fn start() -> Result<(), Status> {
    let port = Port::create()?;
    WORKER.call_once(|| Worker {
        port,
        requested: AtomicBool::new(false),
        timed: AtomicBool::new(false),
    });
    let thread = native_runtime::thread::spawn(b"netd-protocol", run)?;
    drop(thread);
    Ok(())
}

fn worker() -> &'static Worker {
    WORKER.get().expect("protocol worker started before use")
}

/// 请求协议栈尽快处理一轮收发。
pub(crate) fn request_poll() {
    let worker = worker();
    if !worker.requested.swap(true, Ordering::AcqRel) && worker.port.queue(0, 0).is_err() {
        worker.requested.store(false, Ordering::Release);
    }
}

/// 网卡的收发完成与 TX 槽恢复经此请求推进，并开始周期定时检查。
///
/// # Errors
///
/// 闭包分配失败。
pub(crate) fn device_activity() -> Result<device_model::Activity, Status> {
    worker().timed.store(true, Ordering::Release);
    let activity: device_model::Activity =
        alloc::sync::Arc::try_new(request_poll).map_err(|_| native_abi::ERR_NO_MEMORY)?;
    Ok(activity)
}

fn run() {
    let worker = worker();
    loop {
        let deadline = if worker.timed.load(Ordering::Acquire) {
            native_runtime::system::monotonic().saturating_add(POLL_INTERVAL_NS)
        } else {
            DEADLINE_INFINITE
        };
        let requested = worker.port.wait(deadline).is_ok();
        if requested {
            worker.requested.store(false, Ordering::Release);
        }
        if requested || crate::inet::network_work_due() {
            while crate::inet::dispatch_network_work() {}
        }
    }
}
