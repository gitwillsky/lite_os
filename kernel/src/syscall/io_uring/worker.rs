//! 挂起 io_uring 请求链的唯一执行者：一个长期内核线程，把每条挂起的链当作一个 `async fn` future 驱动。
//!
//! 会等待的链（空 pipe、无数据的 socket、尚未触发的 poll 与 timeout、等待 DRAIN）被写成 future，先在提交者
//! 上下文里 poll 一次（[`spawn`]），仍未完成才留给这里。future 里是直线代码——“尝试一次，`EAGAIN` 就 `await`
//! 就绪”——取消就是 `drop` future，它捕获的目标 OFD 和 iovec 随之释放，不需要手写清理。
//!
//! 就绪来源没有回调（readiness 是 task 睡眠/唤醒式的），所以这里不是 waker 驱动的执行器而是 reactor：
//! future 在 `await` 时把“我在等什么”登记到自己的 [`Waiter`]，线程据此构造 `wait_for_poll` 的 key，
//! 条件满足后才重新 `poll` 它。`Context` 因此使用 `Waker::noop()`。
//!
//! 取消不经过命令队列：取消者在登记表里找到匹配的、正在等待的成员，置位它的取消标记并唤醒线程，
//! 成员下次被 poll 时以 `-ECANCELED` 结束（链的后继按链规则处理）。
//!
//! 不用线程池：N 个阻塞读都在等同一个后续写时，固定大小的池会死锁；一个多路复用线程没有这个上限。

use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::{
    future::Future,
    pin::Pin,
    task::{Context, Poll, Waker},
};
use spin::{Mutex, Once};
use syscall_abi::errno;

use super::wait::{Interest, Target, Waiter};
use crate::{
    io_uring::Ring,
    ipc::{Pipe, PipeEnd},
    syscall::poll::{PollWaitKeys, prepare_wait_sources},
    task::{spawn_kernel_thread, wait_for_poll},
};

type ChainFuture = Pin<Box<dyn Future<Output = ()> + Send>>;

/// 一条挂起的链：它的等待登记和 future。
struct Slot {
    waiter: Waiter,
    // OWNER: 链的 future。只有 worker（以及登记前的提交者）poll 它；取消者与等待构造者只碰 `waiter`，
    // 所以持有此锁 poll 时可以安全地回头取消别的链。
    future: Mutex<Option<ChainFuture>>,
}

impl Slot {
    /// poll 一次；返回这条链是否已结束（完成或被丢弃）。
    fn poll(&self) -> bool {
        let mut future = self.future.lock();
        let Some(running) = future.as_mut() else {
            return true;
        };
        let mut context = Context::from_waker(Waker::noop());
        match running.as_mut().poll(&mut context) {
            Poll::Ready(()) => {
                *future = None;
                true
            }
            Poll::Pending => false,
        }
    }

    /// 推进一次；返回这条链是否已结束。
    fn progress(&self) -> bool {
        // ring 关闭即取消：不发布 CQE（没有人会读），丢弃 future 以释放它捕获的目标 OFD。
        if self.waiter.ring().is_closed() {
            *self.future.lock() = None;
            return true;
        }
        self.waiter.attention() && self.poll()
    }
}

struct Shared {
    // OWNER: 所有挂起的链；取消者按它查找目标。只有 worker 移除条目，所以 worker 可以按下标遍历。
    // 缺失时挂起的链没有执行者，阻塞型读写与 poll/timeout 永远不会完成。
    slots: Mutex<Vec<Arc<Slot>>>,
    // OWNER: 唤醒 worker 的合并 readiness Pipe（read, write）。
    wake: (Arc<PipeEnd>, Arc<PipeEnd>),
}

// OWNER: worker 的唯一共享状态；`start` 在 boot 时发布一次。
static SHARED: Once<Shared> = Once::new();

/// 启动 worker 线程；boot 时调用一次，失败即 fail-stop（与其他后台线程一致）。
pub(crate) fn start() -> Result<(), ()> {
    let wake = Pipe::notification_pair()?;
    SHARED.call_once(|| Shared {
        slots: Mutex::new(Vec::new()),
        wake,
    });
    let body = Box::try_new(run).map_err(|_| ())?;
    spawn_kernel_thread("io_uring", body).map_err(|_| ())
}

/// 唤醒 worker 重新扫描（块读与车道作业完成时调用）。
pub(super) fn wake() {
    shared().wake.1.signal_readiness();
}

fn shared() -> &'static Shared {
    SHARED
        .get()
        .expect("io_uring worker is started during boot")
}

/// 登记一条链的 future 并在提交者上下文里 poll 一次；已完成就不留给 worker。
///
/// `build` 在全部分配成功之后才被调用，所以返回 `Err` 时它没有被消耗，调用者仍持有 future 本来要吞掉
/// 的状态，可以逐个以 `-ENOMEM` 完成。
///
/// # Errors
///
/// future 内存、槽或登记表扩容失败时返回 unit。
pub(super) fn spawn<F>(waiter: Waiter, build: impl FnOnce() -> F) -> Result<(), ()>
where
    F: Future<Output = ()> + Send + 'static,
{
    let storage = Box::<F>::try_new_uninit().map_err(|_| ())?;
    let slot = Arc::try_new(Slot {
        waiter,
        future: Mutex::new(None),
    })
    .map_err(|_| ())?;
    let shared = shared();
    {
        let mut slots = shared.slots.lock();
        slots.try_reserve(1).map_err(|_| ())?;
        let future: ChainFuture = Box::into_pin(Box::write(storage, build()));
        *slot.future.lock() = Some(future);
        // 登记时 interest 是 `Starting`：worker 在提交者完成首次 poll 之前不会 poll 它。
        slots.push(slot.clone());
    }
    if slot.poll() {
        let mut slots = shared.slots.lock();
        if let Some(index) = slots.iter().rposition(|entry| Arc::ptr_eq(entry, &slot)) {
            slots.swap_remove(index);
        }
    } else {
        shared.wake.1.signal_readiness();
    }
    Ok(())
}

/// `POLL_REMOVE`/`TIMEOUT_REMOVE`/`ASYNC_CANCEL`：对 `ring` 上匹配的、正在等待的成员置位取消标记。
///
/// # Returns
///
/// 被标记的个数（`all`）、`0`（取消了第一个）或 `-ENOENT`。目标的 `-ECANCELED` CQE 由 worker 稍后发布。
pub(super) fn cancel(ring: &Arc<Ring>, target: &Target, all: bool) -> i32 {
    let shared = shared();
    let mut canceled = 0i32;
    {
        let slots = shared.slots.lock();
        for slot in slots.iter() {
            if Arc::ptr_eq(slot.waiter.ring(), ring) && slot.waiter.cancel(target) {
                canceled += 1;
                if !all {
                    break;
                }
            }
        }
    }
    if canceled > 0 {
        shared.wake.1.signal_readiness();
    }
    match (canceled, all) {
        (0, _) => -(errno::ENOENT as i32),
        (count, true) => count,
        (_, false) => 0,
    }
}

fn slot_at(shared: &Shared, index: usize) -> Option<Arc<Slot>> {
    shared.slots.lock().get(index).cloned()
}

fn run() {
    let shared = shared();
    loop {
        // 先排空唤醒 token 再检查状态：排空之后到达的 spawn/cancel 会重新置位 token。
        shared.wake.0.drain_readiness();
        let mut index = 0;
        while let Some(slot) = slot_at(shared, index) {
            if slot.progress() {
                // 只有 worker 移除条目，所以 `index` 仍指向 `slot`；被换进来的末尾条目在同一下标处理。
                shared.slots.lock().swap_remove(index);
            } else {
                index += 1;
            }
        }
        wait(shared);
    }
}

fn any_attention(shared: &Shared) -> bool {
    let mut index = 0;
    while let Some(slot) = slot_at(shared, index) {
        if slot.waiter.ring().is_closed() || slot.waiter.attention() {
            return true;
        }
        index += 1;
    }
    false
}

fn wait(shared: &Shared) {
    let mut keys = PollWaitKeys::new();
    let mut built = keys.add_notification(&shared.wake.0.pipe()).is_ok();
    let mut deadline: Option<u64> = None;
    let mut index = 0;
    while let Some(slot) = slot_at(shared, index) {
        index += 1;
        // ring 的 readiness Pipe 用来感知 `Ring::close`、新发布的 CQE（带计数的 timeout）与链的完成
        // （DRAIN 接纳）；重复 key 由 wait registry 去重。
        built = built
            && keys
                .add_notification(&slot.waiter.ring().readiness_key().pipe())
                .is_ok();
        if let Some(due) = slot.waiter.link_deadline() {
            deadline = Some(deadline.map_or(due, |current| current.min(due)));
        }
        slot.waiter.with_interest(|interest| match interest {
            Interest::Ofd { ofd, mask, .. } => {
                built = built && keys.add_interest(ofd, *mask, false, None).is_ok();
            }
            Interest::Timer { deadline: due, .. } => {
                deadline = Some(deadline.map_or(*due, |current| current.min(*due)));
            }
            Interest::Capacity(capacity) => {
                built = built
                    && keys
                        .add_socket_source(capacity.wait_source(), false, None)
                        .is_ok();
            }
            Interest::Starting | Interest::None | Interest::Admission { .. } | Interest::Block => {}
        });
    }
    if !built {
        // 无法登记等待就无法保证请求被唤醒：取消全部正在等待的成员（它们下一轮以 `-ECANCELED`
        // 完成），而不是留下永不完成的挂起。
        let mut index = 0;
        while let Some(slot) = slot_at(shared, index) {
            slot.waiter.cancel(&Target::Any);
            index += 1;
        }
        core::hint::spin_loop();
        return;
    }
    let (keys, guards) = keys.finish();
    let mut index = 0;
    while let Some(slot) = slot_at(shared, index) {
        index += 1;
        slot.waiter.with_interest(|interest| match interest {
            Interest::Ofd { ofd, .. } => prepare_wait_sources(ofd),
            Interest::Capacity(capacity) => capacity.prepare_wait(),
            _ => {}
        });
    }
    let _ = wait_for_poll(keys, deadline, || guards.changed() || any_attention(shared));
}
