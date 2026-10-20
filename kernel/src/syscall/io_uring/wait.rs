//! 挂起请求的等待登记与取消标记：future 在 `await` 时把“我在等什么”写进自己的槽，worker 据此构造
//! `wait_for_poll` 的 key，条件满足后才重新 poll 它。
//!
//! 就绪来源没有回调（readiness 是 task 睡眠/唤醒式的 `WAIT_REGISTRY`），所以这里没有 `Waker` 可唤醒：
//! [`Interest::attention`] 由 worker 轮询判定。

use alloc::sync::{Arc, Weak};
use core::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};
use spin::Mutex;

use crate::{file::OpenFileDescription, io_uring::Ring, task::TaskControlBlock};

/// 请求种类：只用于取消匹配和决定 CQE 的发布方式（timeout 的完成不计入事件数）。
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    Io,
    Poll,
    Timeout,
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

/// 链中正在等待的成员。
struct Running {
    user_data: u64,
    kind: Kind,
}

impl Running {
    fn matches(&self, target: &Target) -> bool {
        match *target {
            Target::Any => true,
            Target::UserData(user_data, only) => {
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
        }
    }
}

struct State {
    interest: Interest,
    /// 正在等待的成员；`Now` 成员同步完成，从不出现在这里，所以不能被取消。
    running: Option<Running>,
    /// 取消请求：由 [`Waiter::cancel`] 置位，正在等待的成员观察到后以 `-ECANCELED` 结束。
    canceled: bool,
}

/// 一条挂起链的等待登记：每条链一个，由 future 登记、worker 读取、取消者置位。
#[derive(Clone)]
pub(super) struct Waiter(Arc<WaiterInner>);

struct WaiterInner {
    ring: Arc<Ring>,
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
            state: Mutex::new(State {
                interest: Interest::Starting,
                running: None,
                canceled: false,
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

    pub(super) fn admission(&self, sequence: u64) -> Suspend {
        self.suspend(Interest::Admission {
            ring: self.0.ring.clone(),
            sequence,
        })
    }

    /// 一个会等待的成员开始运行；清除上一个成员遗留的取消标记。
    pub(super) fn begin(&self, user_data: u64, kind: Kind) {
        let mut state = self.0.state.lock();
        state.running = Some(Running { user_data, kind });
        state.canceled = false;
    }

    pub(super) fn end(&self) {
        self.0.state.lock().running = None;
    }

    /// 正在等待的成员匹配 `target` 时标记取消并返回 true。
    pub(super) fn cancel(&self, target: &Target) -> bool {
        let mut state = self.0.state.lock();
        if state.running.as_ref().is_some_and(|running| running.matches(target)) {
            state.canceled = true;
            true
        } else {
            false
        }
    }

    pub(super) fn take_canceled(&self) -> bool {
        core::mem::take(&mut self.0.state.lock().canceled)
    }

    /// 是否该重新 poll 这条链：被取消，或登记的等待条件可能已满足。
    pub(super) fn attention(&self) -> bool {
        let state = self.0.state.lock();
        state.canceled || state.interest.attention()
    }

    /// 在持有状态锁时读取当前等待条件。
    pub(super) fn with_interest<T>(&self, read: impl FnOnce(&Interest) -> T) -> T {
        read(&self.0.state.lock().interest)
    }
}

pub(super) fn event_count_reached(ring: &Ring, target: u32) -> bool {
    (ring.events_posted().wrapping_sub(target) as i32) >= 0
}

fn timeout_due(ring: &Ring, deadline: u64, target_events: Option<u32>) -> bool {
    target_events.is_some_and(|target| event_count_reached(ring, target))
        || deadline <= crate::timer::get_time_ns()
}
