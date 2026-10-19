//! 挂起 io_uring 请求的唯一执行者：一个长期内核线程，把每个挂起请求当作一个 `async fn` future 驱动。
//!
//! 请求在提交者上下文里先做一次尝试（见 `ops`）；会等待的请求（空 pipe、无数据的 socket、尚未触发的
//! poll 与 timeout）被写成 future 交到这里。future 里是直线代码——“尝试一次，`EAGAIN` 就 `await` 就绪”——
//! 取消就是 `drop` future，它捕获的目标 OFD 和 iovec 随之释放，不需要手写清理。
//!
//! 就绪来源没有回调（readiness 是 task 睡眠/唤醒式的），所以这里不是 waker 驱动的执行器而是 reactor：
//! future 在 `await` 时把“我在等什么”登记到自己的 interest 槽，线程据此构造 `wait_for_poll` 的 key，
//! 条件满足后才重新 `poll` 它。`Context` 因此使用 `Waker::noop()`。
//!
//! 不用线程池：N 个阻塞读都在等同一个后续写时，固定大小的池会死锁；一个多路复用线程没有这个上限。

use alloc::{
    boxed::Box,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{
    future::Future,
    pin::Pin,
    task::{Context, Poll, Waker},
};
use spin::{Mutex, Once};
use syscall_abi::errno;

use crate::{
    file::OpenFileDescription,
    io_uring::Ring,
    ipc::{Pipe, PipeEnd},
    syscall::{
        fs::{perform_read, perform_write},
        poll::{PollWaitKeys, prepare_wait_sources},
        user_iovec::UserIoVec,
    },
    task::{TaskControlBlock, spawn_kernel_thread, wait_for_poll},
};

const POLLIN: i16 = 0x001;
const POLLOUT: i16 = 0x004;
const POLLERR: i16 = 0x008;
const POLLHUP: i16 = 0x010;

/// 请求种类：只用于取消匹配和决定 CQE 的发布方式（timeout 的完成不计入事件数）。
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    Io,
    Poll,
    Timeout,
}

/// future 当前在等什么；由 future 登记，worker 读取。
enum Interest {
    /// 刚创建或正在被 poll，没有等待条件。
    None,
    /// 目标 OFD 出现 `mask` 中的事件；`task` 用于以提交线程的上下文判定就绪。
    Ofd {
        ofd: Arc<OpenFileDescription>,
        mask: i16,
        task: Weak<TaskControlBlock>,
    },
    /// deadline 到期，或 ring 上又发布了 `target` 个事件。
    Timer {
        deadline: u64,
        target_events: Option<u32>,
        ring: Arc<Ring>,
    },
}

impl Interest {
    /// 等待条件现在是否可能已满足，即是否该重新 poll future。允许误报（future 会重新检查真实状态）。
    fn attention(&self) -> bool {
        match self {
            Interest::None => true,
            Interest::Ofd { ofd, mask, task } => task
                .upgrade()
                .is_none_or(|task| ofd.poll_events(*mask, task.as_ref()) != 0),
            Interest::Timer {
                deadline,
                target_events,
                ring,
            } => timeout_due(ring, *deadline, *target_events),
        }
    }
}

/// future 登记等待条件的句柄。
#[derive(Clone)]
struct Waiter(Arc<Mutex<Interest>>);

/// 登记一个等待条件并让出一次；worker 在条件满足后再次 poll 时返回 Ready。
struct Suspend {
    waiter: Waiter,
    interest: Option<Interest>,
}

impl Future for Suspend {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<()> {
        match self.interest.take() {
            Some(interest) => {
                *self.waiter.0.lock() = interest;
                Poll::Pending
            }
            None => {
                *self.waiter.0.lock() = Interest::None;
                Poll::Ready(())
            }
        }
    }
}

impl Waiter {
    fn new() -> Result<Self, ()> {
        Arc::try_new(Mutex::new(Interest::None))
            .map(Self)
            .map_err(|_| ())
    }

    fn suspend(&self, interest: Interest) -> Suspend {
        Suspend {
            waiter: self.clone(),
            interest: Some(interest),
        }
    }

    fn ofd_ready(
        &self,
        ofd: &Arc<OpenFileDescription>,
        mask: i16,
        task: &Weak<TaskControlBlock>,
    ) -> Suspend {
        self.suspend(Interest::Ofd {
            ofd: ofd.clone(),
            mask,
            task: task.clone(),
        })
    }
}

fn event_count_reached(ring: &Ring, target: u32) -> bool {
    (ring.events_posted().wrapping_sub(target) as i32) >= 0
}

fn timeout_due(ring: &Ring, deadline: u64, target_events: Option<u32>) -> bool {
    target_events.is_some_and(|target| event_count_reached(ring, target))
        || deadline <= crate::timer::get_time_ns()
}

type RequestFuture = Pin<Box<dyn Future<Output = i32> + Send>>;

/// 一个请求的来源：属于哪个 ring、由哪个线程提交、完成时用哪个 user_data。
///
/// 这四项总是一起出现，打包后构造函数不再带一长串位置参数。
pub(super) struct Origin {
    pub(super) ring: Arc<Ring>,
    /// 提交线程的弱引用：强引用会延长其 TCB 寿命。
    pub(super) task: Weak<TaskControlBlock>,
    pub(super) user_data: u64,
    pub(super) skip_success: bool,
}

/// 一个已提交、尚未完成的请求：元数据、等待条件登记槽和它的 future。
pub(super) struct Request {
    ring: Arc<Ring>,
    user_data: u64,
    skip_success: bool,
    kind: Kind,
    waiter: Waiter,
    /// 新收编的请求必须先 poll 一次，才能登记它的等待条件。
    fresh: bool,
    future: RequestFuture,
}

fn request(
    origin: &Origin,
    kind: Kind,
    build: impl FnOnce(Waiter) -> Result<RequestFuture, ()>,
) -> Result<Request, ()> {
    let waiter = Waiter::new()?;
    let future = build(waiter.clone())?;
    Ok(Request {
        ring: origin.ring.clone(),
        user_data: origin.user_data,
        skip_success: origin.skip_success,
        kind,
        waiter,
        fresh: true,
        future,
    })
}

fn boxed(future: impl Future<Output = i32> + Send + 'static) -> Result<RequestFuture, ()> {
    Box::try_new(future)
        .map(|boxed| -> RequestFuture { Box::into_pin(boxed) })
        .map_err(|_| ())
}

/// 阻塞型读写：尝试一次，`EAGAIN` 就等目标 OFD 就绪再试。
pub(super) fn read_write_request(
    origin: &Origin,
    ofd: Arc<OpenFileDescription>,
    vectors: Vec<UserIoVec>,
    total: usize,
    write: bool,
) -> Result<Request, ()> {
    let task = origin.task.clone();
    request(origin, Kind::Io, |waiter| {
        // ERR/HUP 一并关注，使对端关闭也能唤醒并让重试读到 EOF/EPIPE。
        let mask = (if write { POLLOUT } else { POLLIN }) | POLLERR | POLLHUP;
        boxed(async move {
            loop {
                // 只在这一小段持有提交线程的强引用，不跨 `.await`：强引用会延长其 TCB 寿命。
                let Some(strong) = task.upgrade() else {
                    return -(errno::ECANCELED as i32);
                };
                let result = if write {
                    perform_write(&strong, &ofd, &vectors, total, true)
                } else {
                    perform_read(&strong, &ofd, &vectors, total, true)
                };
                drop(strong);
                if result != -errno::EAGAIN {
                    return result as i32;
                }
                waiter.ofd_ready(&ofd, mask, &task).await;
            }
        })
    })
}

/// `POLL_ADD`：一次性，目标 OFD 出现 `mask` 中任一事件时以 revents 完成。
pub(super) fn poll_request(
    origin: &Origin,
    ofd: Arc<OpenFileDescription>,
    mask: i16,
) -> Result<Request, ()> {
    let task = origin.task.clone();
    request(origin, Kind::Poll, |waiter| {
        boxed(async move {
            loop {
                let Some(strong) = task.upgrade() else {
                    return -(errno::ECANCELED as i32);
                };
                let ready = ofd.poll_events(mask, strong.as_ref());
                drop(strong);
                if ready != 0 {
                    return i32::from(ready);
                }
                waiter.ofd_ready(&ofd, mask, &task).await;
            }
        })
    })
}

/// `TIMEOUT`：绝对 monotonic deadline，或“除 timeout 外又发布 N 个 CQE”（`target_events`）。
pub(super) fn timeout_request(
    origin: &Origin,
    deadline: u64,
    target_events: Option<u32>,
    etime_success: bool,
) -> Result<Request, ()> {
    let watched = origin.ring.clone();
    request(origin, Kind::Timeout, |waiter| {
        boxed(async move {
            loop {
                // 先看计数：满足计数以 0 完成；否则只可能是 deadline 到期。
                if target_events.is_some_and(|target| event_count_reached(&watched, target)) {
                    return 0;
                }
                if deadline <= crate::timer::get_time_ns() {
                    return if etime_success {
                        0
                    } else {
                        -(errno::ETIME as i32)
                    };
                }
                waiter
                    .suspend(Interest::Timer {
                        deadline,
                        target_events,
                        ring: watched.clone(),
                    })
                    .await;
            }
        })
    })
}

impl Request {
    fn finish(&self, result: i32) {
        match self.kind {
            Kind::Timeout => self
                .ring
                .complete_timeout(self.user_data, self.skip_success, result),
            _ => self
                .ring
                .complete(self.user_data, self.skip_success, result),
        }
    }

    /// 该请求现在是否需要被 poll：ring 已关闭，或它登记的等待条件可能已满足。
    fn needs_attention(&self) -> bool {
        self.fresh || self.ring.is_closed() || self.waiter.0.lock().attention()
    }

    /// 推进一次；返回请求是否已结束（完成或取消）。
    fn progress(&mut self) -> bool {
        // ring 关闭即取消：不发布 CQE（没有人会读），丢弃本请求以释放 future 捕获的目标 OFD。
        if self.ring.is_closed() {
            return true;
        }
        if !self.needs_attention() {
            return false;
        }
        self.fresh = false;
        let mut context = Context::from_waker(Waker::noop());
        match self.future.as_mut().poll(&mut context) {
            Poll::Ready(result) => {
                self.finish(result);
                true
            }
            Poll::Pending => false,
        }
    }
}

/// 取消的目标范围。
#[derive(Clone, Copy)]
pub(super) enum Only {
    Any,
    Poll,
    Timeout,
}

pub(super) enum Target {
    /// 匹配 user_data，并限定请求种类（`POLL_REMOVE`/`TIMEOUT_REMOVE` 只能取消各自种类）。
    UserData(u64, Only),
    /// `IORING_ASYNC_CANCEL_ANY`：匹配该 ring 的任意请求。
    Any,
}

/// `POLL_REMOVE`/`TIMEOUT_REMOVE`/`ASYNC_CANCEL`：由 worker 执行并发布结果。
pub(super) struct Cancel {
    pub(super) ring: Arc<Ring>,
    /// 发起取消的请求自己的 user_data 与 skip 标志。
    pub(super) user_data: u64,
    pub(super) skip_success: bool,
    pub(super) target: Target,
    /// `IORING_ASYNC_CANCEL_ALL`：取消全部匹配项并返回个数，否则只取消第一个。
    pub(super) all: bool,
}

impl Cancel {
    fn matches(&self, request: &Request) -> bool {
        if !Arc::ptr_eq(&self.ring, &request.ring) {
            return false;
        }
        match self.target {
            Target::Any => true,
            Target::UserData(user_data, only) => {
                request.user_data == user_data
                    && matches!(
                        (only, request.kind),
                        (Only::Any, _) | (Only::Poll, Kind::Poll) | (Only::Timeout, Kind::Timeout)
                    )
            }
        }
    }

    /// 取消 `active` 里匹配的请求并发布结果：目标得到 `-ECANCELED`，本请求得到个数（ALL）、0 或 `-ENOENT`。
    fn apply(&self, active: &mut Vec<Request>) {
        let mut canceled = 0i32;
        let mut index = 0;
        while index < active.len() {
            if self.matches(&active[index]) {
                // drop future：它持有的目标 OFD、iovec 随之释放。
                active.swap_remove(index).finish(-(errno::ECANCELED as i32));
                canceled += 1;
                if !self.all {
                    break;
                }
            } else {
                index += 1;
            }
        }
        let result = match (canceled, self.all) {
            (0, _) => -(errno::ENOENT as i32),
            (count, true) => count,
            (_, false) => 0,
        };
        self.ring
            .complete(self.user_data, self.skip_success, result);
    }
}

struct Shared {
    // OWNER: 提交者与 worker 之间的交接队列；缺失时提交者只能直接改 worker 私有的 active 表。
    incoming: Mutex<Vec<Request>>,
    // OWNER: 待执行的取消。与 incoming 分开，使 worker 能先收编请求、再执行取消，保证同一批里
    // “提交 A，再取消 A”的顺序成立。
    commands: Mutex<Vec<Cancel>>,
    // OWNER: 唤醒 worker 的合并 readiness Pipe（read, write）。
    wake: (Arc<PipeEnd>, Arc<PipeEnd>),
}

// OWNER: worker 的唯一共享状态；`start` 在 boot 时发布一次。缺失时挂起的请求没有执行者，
// 阻塞型读写与 poll/timeout 永远不会完成。
static SHARED: Once<Shared> = Once::new();

/// 启动 worker 线程；boot 时调用一次，失败即 fail-stop（与其他后台线程一致）。
pub(crate) fn start() -> Result<(), ()> {
    let wake = Pipe::notification_pair()?;
    SHARED.call_once(|| Shared {
        incoming: Mutex::new(Vec::new()),
        commands: Mutex::new(Vec::new()),
        wake,
    });
    let body = Box::try_new(run).map_err(|_| ())?;
    spawn_kernel_thread("io_uring", body).map_err(|_| ())
}

fn shared() -> &'static Shared {
    SHARED
        .get()
        .expect("io_uring worker is started during boot")
}

/// 把一个会等待的请求交给 worker。
///
/// # Errors
///
/// 队列扩容失败时返回 unit，请求被丢弃，调用者以 `ENOMEM` 完成它。
pub(super) fn park(request: Request) -> Result<(), ()> {
    let shared = shared();
    let mut incoming = shared.incoming.lock();
    incoming.try_reserve(1).map_err(|_| ())?;
    incoming.push(request);
    drop(incoming);
    shared.wake.1.signal_readiness();
    Ok(())
}

/// 把一个取消交给 worker 执行。
///
/// # Errors
///
/// 队列扩容失败时返回 unit，调用者以 `ENOMEM` 完成取消请求。
pub(super) fn cancel(command: Cancel) -> Result<(), ()> {
    let shared = shared();
    let mut commands = shared.commands.lock();
    commands.try_reserve(1).map_err(|_| ())?;
    commands.push(command);
    drop(commands);
    shared.wake.1.signal_readiness();
    Ok(())
}

fn run() {
    let shared = shared();
    let mut active: Vec<Request> = Vec::new();
    loop {
        // 先排空唤醒 token 再检查状态：排空之后到达的 park/cancel 会重新置位 token。
        shared.wake.0.drain_readiness();
        adopt(shared, &mut active);
        apply_cancels(shared, &mut active);
        active.retain_mut(|request| !request.progress());
        wait(shared, &mut active);
    }
}

/// 收编 incoming；扩容失败时以 `ENOMEM` 结束这批请求，而不是丢失它们。
fn adopt(shared: &Shared, active: &mut Vec<Request>) {
    let mut incoming = shared.incoming.lock();
    if incoming.is_empty() {
        return;
    }
    if active.try_reserve(incoming.len()).is_err() {
        for request in incoming.drain(..) {
            request.finish(-(errno::ENOMEM as i32));
        }
        return;
    }
    active.append(&mut incoming);
}

fn apply_cancels(shared: &Shared, active: &mut Vec<Request>) {
    let commands: Vec<Cancel> = core::mem::take(&mut *shared.commands.lock());
    for command in commands {
        command.apply(active);
    }
}

fn wait(shared: &Shared, active: &mut Vec<Request>) {
    let mut keys = PollWaitKeys::new();
    let mut built = keys.add_notification(&shared.wake.0.pipe()).is_ok();
    let mut deadline: Option<u64> = None;
    for request in active.iter() {
        // ring 的 readiness Pipe 用来感知 `Ring::close` 与新发布的 CQE（带计数的 timeout 依赖它）；
        // 重复 key 由 wait registry 去重。
        built = built
            && keys
                .add_notification(&request.ring.readiness_key().pipe())
                .is_ok();
        match &*request.waiter.0.lock() {
            Interest::None => {}
            Interest::Ofd { ofd, mask, .. } => {
                built = built && keys.add_interest(ofd, *mask, false, None).is_ok();
            }
            Interest::Timer { deadline: due, .. } => {
                deadline = Some(deadline.map_or(*due, |current| current.min(*due)));
            }
        }
    }
    if !built {
        // 无法登记等待就无法保证请求被唤醒：失败全部请求，而不是留下永不完成的挂起。
        for request in active.drain(..) {
            request.finish(-(errno::ENOMEM as i32));
        }
        core::hint::spin_loop();
        return;
    }
    let (keys, guards) = keys.finish();
    for request in active.iter() {
        if let Interest::Ofd { ofd, .. } = &*request.waiter.0.lock() {
            prepare_wait_sources(ofd);
        }
    }
    let _ = wait_for_poll(keys, deadline, || {
        guards.changed()
            || !shared.incoming.lock().is_empty()
            || !shared.commands.lock().is_empty()
            || active.iter().any(Request::needs_attention)
    });
}
