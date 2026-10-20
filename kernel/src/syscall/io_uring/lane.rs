//! 阻塞车道：一组按需创建的内核线程，执行可能睡眠在磁盘 I/O 上的 io_uring 作业。
//!
//! reactor 线程多路复用全部挂起的链，它一睡眠所有链都停；所以会睡眠的工作（regular 文件写、fsync、
//! 当前位置读、无法不阻塞完成的读）都交给这里。作业拥有执行所需的全部数据，并以提交线程的 task 执行
//! （`Weak`，线程已退出则 `-ECANCELED`）。
//!
//! 与 Linux io-wq 同构：共享一个 FIFO，没有空闲 worker 且未达上限时新建一个，所以一个慢作业只占住一个
//! worker，写与 fsync 可以重叠。同一条链内的成员本来就串行（链同一时刻至多一个成员在等待）；不同链之间
//! io_uring 不保证顺序，因此不需要 per-inode 排序，文件内部的一致性由 inode 锁保证。

use alloc::{
    boxed::Box,
    collections::VecDeque,
    sync::{Arc, Weak},
    vec::Vec,
};
use spin::{Mutex, Once};
use syscall_abi::errno;

use crate::{
    fs::CompletionNotify,
    ipc::{Pipe, PipeEnd},
    syscall::poll::PollWaitKeys,
    task::{TaskControlBlock, spawn_kernel_thread, wait_for_poll},
};

/// worker 数量上限；缺失该上限时，大量慢作业会各自创建一个内核线程（每个带内核栈）而耗尽内存。
/// 达到上限后作业在共享 FIFO 里排队，行为退化为有界并发。
const MAX_WORKERS: usize = 8;

pub(super) type Work = Box<dyn FnOnce(&TaskControlBlock) -> isize + Send>;

/// 一个已提交作业的结果槽；提交者与车道线程各持一个引用。
pub(super) struct Ticket {
    // OWNER: 作业结果；车道线程写入后通知提交者。缺失时提交者无法取回结果。
    result: Mutex<Option<isize>>,
    notify: Arc<dyn CompletionNotify>,
}

impl Ticket {
    /// 作业已完成时取走结果。
    pub(super) fn take(&self) -> Option<isize> {
        self.result.lock().take()
    }
}

struct Job {
    work: Work,
    task: Weak<TaskControlBlock>,
    ticket: Arc<Ticket>,
}

/// 一个 worker 的唤醒通道；`submit` 在持有 [`Pool`] 锁时认领空闲 worker 并向它发信号。
struct Worker {
    wake: (Arc<PipeEnd>, Arc<PipeEnd>),
}

struct Pool {
    // OWNER: 待执行作业的 FIFO。缺失时车道没有输入；顺序即提交顺序。
    queue: VecDeque<Job>,
    // OWNER: 已创建的 worker 与它们是否空闲（下标一一对应）。空闲标志只在本锁下改变：
    // worker 在观察到队列为空的同一临界区里置空闲，submit 在入队的同一临界区里认领，
    // 因此入队与置空闲不会交错出“作业在队列里而所有 worker 都在睡”。
    workers: Vec<(Arc<Worker>, bool)>,
}

// OWNER: 车道的唯一共享状态；`start` 在 boot 时发布一次。
static POOL: Once<Mutex<Pool>> = Once::new();

/// 初始化车道（尚不创建 worker，首个作业到来时才创建）。
pub(super) fn start() -> Result<(), ()> {
    POOL.call_once(|| {
        Mutex::new(Pool {
            queue: VecDeque::new(),
            workers: Vec::new(),
        })
    });
    Ok(())
}

fn pool() -> &'static Mutex<Pool> {
    POOL.get().expect("io_uring lane is started during boot")
}

/// 提交一个会睡眠的作业；完成时调用 `notify`。
///
/// 丢弃返回的 [`Ticket`] 即取消：尚未开始的作业不会执行，已开始的作业跑完后结果被丢弃。
///
/// # Errors
///
/// 作业、结果槽或队列扩容失败时返回 unit，调用者以 `-ENOMEM` 完成。
pub(super) fn submit(
    work: Work,
    task: Weak<TaskControlBlock>,
    notify: Arc<dyn CompletionNotify>,
) -> Result<Arc<Ticket>, ()> {
    let ticket = Arc::try_new(Ticket {
        result: Mutex::new(None),
        notify,
    })
    .map_err(|_| ())?;
    let mut pool = pool().lock();
    pool.queue.try_reserve(1).map_err(|_| ())?;
    pool.workers.try_reserve(1).map_err(|_| ())?;
    pool.queue.push_back(Job {
        work,
        task,
        ticket: ticket.clone(),
    });
    if let Some((worker, idle)) = pool.workers.iter_mut().find(|(_, idle)| *idle) {
        *idle = false;
        worker.wake.1.signal_readiness();
        return Ok(ticket);
    }
    if pool.workers.len() >= MAX_WORKERS {
        // 全忙：某个 worker 完成当前作业后会从共享 FIFO 取走它。
        return Ok(ticket);
    }
    match spawn_worker() {
        Ok(worker) => pool.workers.push((worker, false)),
        // 已有 worker 时作业终将被取走；一个 worker 都没有则没人执行，撤回作业并报告失败。
        Err(()) if pool.workers.is_empty() => {
            pool.queue.pop_back();
            return Err(());
        }
        Err(()) => {}
    }
    Ok(ticket)
}

/// 创建一个新 worker，它启动后立即检查共享 FIFO（调用者已把作业入队）。
fn spawn_worker() -> Result<Arc<Worker>, ()> {
    let wake = Pipe::notification_pair()?;
    let worker = Arc::try_new(Worker { wake }).map_err(|_| ())?;
    let body = {
        let worker = worker.clone();
        Box::try_new(move || run(&worker)).map_err(|_| ())?
    };
    spawn_kernel_thread("io_uring_lane", body).map_err(|_| ())?;
    Ok(worker)
}

fn run(worker: &Arc<Worker>) {
    loop {
        worker.wake.0.drain_readiness();
        loop {
            let mut guard = pool().lock();
            let Some(job) = guard.queue.pop_front() else {
                // 队列为空：在同一临界区里标记自己空闲（见 `Pool::workers`）。
                let entry = guard
                    .workers
                    .iter_mut()
                    .find(|(candidate, _)| Arc::ptr_eq(candidate, worker))
                    .expect("a running worker is registered in the pool");
                entry.1 = true;
                break;
            };
            drop(guard);
            execute(job);
        }
        let mut keys = PollWaitKeys::new();
        if keys.add_notification(&worker.wake.0.pipe()).is_err() {
            core::hint::spin_loop();
            continue;
        }
        let (keys, guards) = keys.finish();
        let _ = wait_for_poll(keys, None, || guards.changed());
    }
}

fn execute(job: Job) {
    let Job { work, task, ticket } = job;
    // 提交者已经丢弃了票据：没有人等结果，不必执行。
    if Arc::strong_count(&ticket) == 1 {
        return;
    }
    let result = match task.upgrade() {
        Some(task) => work(&task),
        None => -errno::ECANCELED,
    };
    *ticket.result.lock() = Some(result);
    ticket.notify.notify();
}
