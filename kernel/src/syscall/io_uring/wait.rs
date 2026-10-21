//! 挂起请求的等待登记与取消标记：future 在 `await` 时把“我在等什么”写进自己的槽，worker 据此构造
//! `wait_for_poll` 的 key，条件满足后才重新 poll 它。
//!
//! 就绪来源没有回调（readiness 是 task 睡眠/唤醒式的 `WAIT_REGISTRY`），所以这里没有 `Waker` 可唤醒：
//! [`Interest::attention`] 由 worker 轮询判定。

use alloc::sync::{Arc, Weak};
use core::{
    future::Future,
    pin::Pin,
    sync::atomic::{AtomicBool, Ordering},
    task::{Context, Poll},
};
use spin::Mutex;

use crate::{
    file::OpenFileDescription, fs::CompletionNotify, io_uring::Ring, socket::SocketCapacityWait,
    task::TaskControlBlock,
};

/// 请求种类：只用于取消匹配和决定 CQE 的发布方式（timeout 的完成不计入事件数）。
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    Io,
    Poll,
    Timeout,
}

/// 一次取消尝试的结果。
pub(super) enum Cancel {
    NoMatch,
    Canceled,
    /// 匹配的成员已经在车道线程上执行：不能取消，会照常完成（Linux `-EALREADY`）。
    Running,
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
    /// `IORING_ASYNC_CANCEL_FD`/`_OP`（可与 `_USERDATA` 组合）：给出的条件必须全部满足（Linux `io_cancel_req_match`）。
    /// `file` 在 issue 时才由 fd 解析成 OFD（链里前面的成员可能改变 fd 表）。
    Match {
        user_data: Option<u64>,
        opcode: Option<u8>,
        file: Option<Arc<OpenFileDescription>>,
    },
}

/// 链中正在等待的成员。
struct Running {
    user_data: u64,
    kind: Kind,
    opcode: u8,
    /// 成员操作的 OFD（没有 fd 的成员如 timeout 为 `None`）；按 fd 取消时按同一个 OFD 匹配。
    file: Option<Arc<OpenFileDescription>>,
}

impl Running {
    fn matches(&self, target: &Target) -> bool {
        match target {
            Target::Any => true,
            Target::Match {
                user_data,
                opcode,
                file,
            } => {
                user_data.is_none_or(|expected| self.user_data == expected)
                    && opcode.is_none_or(|expected| self.opcode == expected)
                    && file.as_ref().is_none_or(|expected| {
                        self.file
                            .as_ref()
                            .is_some_and(|running| Arc::ptr_eq(running, expected))
                    })
            }
            &Target::UserData(user_data, only) => {
                self.user_data == user_data
                    && matches!(
                        (only, self.kind),
                        (Only::Any, _) | (Only::Poll, Kind::Poll) | (Only::Timeout, Kind::Timeout)
                    )
            }
        }
    }
}

/// future 当前在等什么；由 future 登记，worker 读取。
pub(super) enum Interest {
    /// 刚创建，提交者正在首次 poll 它：worker 不得抢先 poll。
    Starting,
    /// 正在被 poll，没有等待条件。
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
    /// 等待链被接纳（DRAIN 顺序）：序号为 `sequence` 的链可以开始。
    Admission { ring: Arc<Ring>, sequence: u64 },
    /// 等块层或阻塞车道的完成通知（[`Waiter::notifier`]）。
    Block,
    /// 等 AF_UNIX 目标（listener backlog）恢复容量。
    Capacity(Arc<SocketCapacityWait>),
}

impl Interest {
    /// 等待条件现在是否可能已满足，即是否该重新 poll future。允许误报（future 会重新检查真实状态）。
    fn attention(&self) -> bool {
        match self {
            Interest::Starting => false,
            Interest::None => true,
            Interest::Ofd { ofd, mask, task } => task
                .upgrade()
                .is_none_or(|task| ofd.poll_events(*mask, task.as_ref()) != 0),
            Interest::Timer {
                deadline,
                target_events,
                ring,
            } => timeout_due(ring, *deadline, *target_events),
            Interest::Admission { ring, sequence } => ring.may_start(*sequence),
            // 由 `Waiter::attention` 结合通知标志判定。
            Interest::Block => false,
            Interest::Capacity(capacity) => capacity.is_ready(),
        }
    }
}

struct State {
    interest: Interest,
    /// 正在等待的成员；`Now` 成员同步完成，从不出现在这里，所以不能被取消。
    running: Option<Running>,
    /// 取消请求：由 [`Waiter::cancel`] 置位，正在等待的成员观察到后以 `-ECANCELED` 结束。
    canceled: bool,
    /// 正在车道上执行/排队的成员的票据；取消据此区分“尚未开始，可取消”与“已开始，只能等它完成”。
    lane: Option<Arc<super::lane::Ticket>>,
    /// `LINK_TIMEOUT` 的 deadline：到期后正在等待的成员被当作取消，并记下是定时器造成的。
    /// 缺失时 reactor 不会在 deadline 唤醒这条链，被约束的成员会一直等到自己的就绪条件。
    link_deadline: Option<u64>,
    /// 定时器到期并取消了成员；决定 timeout 自己的 CQE 是 `-ETIME` 还是 `-ECANCELED`。
    timed_out: bool,
}

/// 一条挂起链的等待登记：每条链一个，由 future 登记、worker 读取、取消者置位。
#[derive(Clone)]
pub(super) struct Waiter(Arc<WaiterInner>);

struct WaiterInner {
    ring: Arc<Ring>,
    // OWNER: 块读/车道作业完成时置位（可在 deferred completion 上下文里无锁调用），future 在 `step`
    // 之前清除。置位发生在清除之后即不会丢：下一次 attention 看到它。缺失时 `Interest::Block` 的
    // future 要么永不被重新 poll，要么让 reactor 空转。
    notified: AtomicBool,
    // OWNER: 等待条件、正在运行的成员与取消标记。只在 worker、创建它的提交者与取消者之间短暂持有，
    // 且从不跨 `.await`；缺失时 worker 不知道该等什么、取消者无从找到目标。
    state: Mutex<State>,
}

/// 登记一个等待条件并让出一次；worker 在条件满足后再次 poll 时返回 Ready。
pub(super) struct Suspend {
    waiter: Waiter,
    interest: Option<Interest>,
}

impl Future for Suspend {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<()> {
        match self.interest.take() {
            Some(interest) => {
                self.waiter.0.state.lock().interest = interest;
                Poll::Pending
            }
            None => {
                self.waiter.0.state.lock().interest = Interest::None;
                Poll::Ready(())
            }
        }
    }
}

impl Waiter {
    pub(super) fn new(ring: &Arc<Ring>) -> Result<Self, ()> {
        Arc::try_new(WaiterInner {
            ring: ring.clone(),
            notified: AtomicBool::new(false),
            state: Mutex::new(State {
                interest: Interest::Starting,
                running: None,
                canceled: false,
                lane: None,
                link_deadline: None,
                timed_out: false,
            }),
        })
        .map(Self)
        .map_err(|_| ())
    }

    pub(super) fn ring(&self) -> &Arc<Ring> {
        &self.0.ring
    }

    fn suspend(&self, interest: Interest) -> Suspend {
        Suspend {
            waiter: self.clone(),
            interest: Some(interest),
        }
    }

    pub(super) fn ofd_ready(
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

    pub(super) fn timer(
        &self,
        deadline: u64,
        target_events: Option<u32>,
        ring: &Arc<Ring>,
    ) -> Suspend {
        self.suspend(Interest::Timer {
            deadline,
            target_events,
            ring: ring.clone(),
        })
    }

    pub(super) fn capacity(&self, capacity: &Arc<SocketCapacityWait>) -> Suspend {
        self.suspend(Interest::Capacity(capacity.clone()))
    }

    pub(super) fn admission(&self, sequence: u64) -> Suspend {
        self.suspend(Interest::Admission {
            ring: self.0.ring.clone(),
            sequence,
        })
    }

    /// 块层与车道完成时调用的通知；持有它会保活这条链的登记。
    pub(super) fn notifier(&self) -> Arc<dyn CompletionNotify> {
        self.0.clone()
    }

    /// 在每次 `step` 之前调用：之后的通知都会让 [`Waiter::block`] 立即被重新 poll。
    pub(super) fn clear_notified(&self) {
        self.0.notified.store(false, Ordering::Release);
    }

    pub(super) fn block(&self) -> Suspend {
        self.suspend(Interest::Block)
    }

    /// 一个会等待的成员开始运行；清除上一个成员遗留的取消标记。
    pub(super) fn begin(
        &self,
        user_data: u64,
        kind: Kind,
        opcode: u8,
        file: Option<Arc<OpenFileDescription>>,
    ) {
        let mut state = self.0.state.lock();
        state.running = Some(Running {
            user_data,
            kind,
            opcode,
            file,
        });
        state.canceled = false;
    }

    pub(super) fn end(&self) {
        let mut state = self.0.state.lock();
        state.running = None;
        state.lane = None;
    }

    /// 正在等待的成员是一个车道作业：之后的取消要先问它是否还能取消。
    pub(super) fn attach_lane(&self, ticket: Arc<super::lane::Ticket>) {
        self.0.state.lock().lane = Some(ticket);
    }

    /// 正在等待的成员匹配 `target` 时尝试取消它。
    pub(super) fn cancel(&self, target: &Target) -> Cancel {
        let mut state = self.0.state.lock();
        if !state
            .running
            .as_ref()
            .is_some_and(|running| running.matches(target))
        {
            return Cancel::NoMatch;
        }
        // 车道作业一旦开始就不能被取消：它的副作用会发生，必须报告真实结果。
        if state
            .lane
            .as_ref()
            .is_some_and(|ticket| !ticket.try_cancel())
        {
            return Cancel::Running;
        }
        state.canceled = true;
        Cancel::Canceled
    }

    pub(super) fn take_canceled(&self) -> bool {
        let mut state = self.0.state.lock();
        if state.link_deadline.is_some_and(link_due) {
            state.link_deadline = None;
            state.timed_out = true;
            state.canceled = false;
            return true;
        }
        core::mem::take(&mut state.canceled)
    }

    /// 给正在开始等待的成员装上 `LINK_TIMEOUT` 定时器。
    pub(super) fn arm_link_timeout(&self, deadline: u64) {
        let mut state = self.0.state.lock();
        state.link_deadline = Some(deadline);
        state.timed_out = false;
    }

    /// 成员结束：撤下定时器，返回它是否因定时器到期而被取消。
    pub(super) fn disarm_link_timeout(&self) -> bool {
        let mut state = self.0.state.lock();
        state.link_deadline = None;
        core::mem::take(&mut state.timed_out)
    }

    /// 当前装着的 `LINK_TIMEOUT` deadline；reactor 据此决定睡眠上限。
    pub(super) fn link_deadline(&self) -> Option<u64> {
        self.0.state.lock().link_deadline
    }

    /// 是否该重新 poll 这条链：被取消，或登记的等待条件可能已满足。
    pub(super) fn attention(&self) -> bool {
        let state = self.0.state.lock();
        state.canceled
            || state.link_deadline.is_some_and(link_due)
            || state.interest.attention()
            || (matches!(state.interest, Interest::Block)
                && self.0.notified.load(Ordering::Acquire))
    }

    /// 在持有状态锁时读取当前等待条件。
    pub(super) fn with_interest<T>(&self, read: impl FnOnce(&Interest) -> T) -> T {
        read(&self.0.state.lock().interest)
    }
}

impl CompletionNotify for WaiterInner {
    fn notify(&self) {
        self.notified.store(true, Ordering::Release);
        super::worker::wake();
    }
}

fn link_due(deadline: u64) -> bool {
    deadline <= crate::timer::get_time_ns()
}

pub(super) fn event_count_reached(ring: &Ring, target: u32) -> bool {
    (ring.events_posted().wrapping_sub(target) as i32) >= 0
}

fn timeout_due(ring: &Ring, deadline: u64, target_events: Option<u32>) -> bool {
    target_events.is_some_and(|target| event_count_reached(ring, target))
        || deadline <= crate::timer::get_time_ns()
}
