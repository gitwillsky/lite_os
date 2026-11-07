//! 有界阻塞任务；同步 I/O 和计算不在 UI Future 的 poll 中执行。
use crate::{Error, Result};
use std::{
    collections::VecDeque,
    future::Future,
    pin::Pin,
    sync::{Arc, Condvar, Mutex},
    task::{Context, Poll, Waker},
    thread,
    time::Duration,
};
type Job = Box<dyn FnOnce() + Send>;
const WORKERS: usize = 2;
const QUEUE: usize = 64;

/// 可在 UI Future 内提交阻塞工作的句柄；池按需增长，空闲后释放全部线程。
///
/// 最多两个并发任务和 64 个排队任务；最后一个句柄释放后仍完成已接收任务，不在 UI 上 join。
pub struct Background {
    pool: Arc<Pool>,
}
struct Pool {
    // OWNER: 准入、工作者计数与空闲退休在同一锁内；工作者不能在提交后悄然退出并遗留任务。
    // 不在锁内执行/释放 job 或唤醒 UI；Condvar 仅用于无任务的等待，不周期轮询任务。
    state: Mutex<PoolState>,
    ready: Condvar,
}
struct PoolState {
    jobs: VecDeque<Job>,
    workers: usize,
    idle: usize,
    // OWNER: 只有 Background 句柄拥有提交权；工作者保活 Arc 不会阻止最后提交者触发退出。
    submitters: usize,
}
impl Clone for Background {
    fn clone(&self) -> Self {
        let mut state = self.pool.state.lock().expect("worker queue poisoned");
        state.submitters = state
            .submitters
            .checked_add(1)
            .expect("too many submitters");
        Self {
            pool: self.pool.clone(),
        }
    }
}
impl Drop for Background {
    fn drop(&mut self) {
        let last = {
            let mut state = self.pool.state.lock().expect("worker queue poisoned");
            state.submitters -= 1;
            state.submitters == 0
        };
        if last {
            self.pool.ready.notify_all();
        }
    }
}
impl Pool {
    fn work(&self) {
        loop {
            let job = {
                let mut state = self.state.lock().expect("worker queue poisoned");
                if state.jobs.is_empty() && state.submitters != 0 {
                    state.idle += 1;
                    let (mut waiting, _) = self
                        .ready
                        .wait_timeout_while(state, Duration::from_secs(5), |state| {
                            state.jobs.is_empty() && state.submitters != 0
                        })
                        .expect("worker queue poisoned");
                    waiting.idle -= 1;
                    state = waiting;
                }
                match state.jobs.pop_front() {
                    Some(job) => job,
                    None => {
                        // 退休与提交同锁；下一项要么已有接收者，要么由提交者新建线程。
                        state.workers -= 1;
                        return;
                    }
                }
            };
            job();
        }
    }
    fn submit(self: &Arc<Self>, job: Job) -> Result<()> {
        let mut state = self.state.lock().expect("worker queue poisoned");
        if state.jobs.len() == QUEUE {
            return Err(Error {
                code: classic_ui_abi::FAILED,
                message: "background queue is full".into(),
            });
        }
        // 冷路径创建线程时持准入锁，保证创建失败不发表无人接收的任务。
        // 新线程首先取同一锁；只有创建成功才计入 workers，任务执行永远在锁外。
        if state.idle <= state.jobs.len() && state.workers < WORKERS {
            let pool = self.clone();
            match thread::Builder::new()
                .name("ui-worker".into())
                .spawn(move || pool.work())
            {
                Ok(_) => state.workers += 1,
                Err(error) if state.workers == 0 => return Err(crate::io_error(error)),
                Err(_) => {} // 已有工作者仍能推进；扩容失败不丢弃已经可接收的任务。
            }
        }
        state.jobs.push_back(job);
        drop(state);
        self.ready.notify_one();
        Ok(())
    }
}
impl Background {
    pub(crate) fn new() -> Result<Self> {
        let mut jobs = VecDeque::new();
        jobs.try_reserve_exact(QUEUE).map_err(|_| Error {
            code: classic_ui_abi::FAILED,
            message: "background queue allocation failed".into(),
        })?;
        Ok(Self {
            pool: Arc::new(Pool {
                state: Mutex::new(PoolState {
                    jobs,
                    workers: 0,
                    idle: 0,
                    submitters: 1,
                }),
                ready: Condvar::new(),
            }),
        })
    }
    /// 提交有界阻塞工作；队列满或 worker 已停止立即返回错误。
    /// 取消 Future 可跳过尚未执行的工作，已发生的副作用不会回滚。
    pub fn run<T: Send + 'static>(
        &self,
        work: impl FnOnce() -> T + Send + 'static,
    ) -> Result<Blocking<T>> {
        let state = Arc::new(Mutex::new(Completion {
            value: None,
            waker: None,
            cancelled: false,
        }));
        let output = state.clone();
        let job = Box::new(move || {
            if output.lock().expect("completion poisoned").cancelled {
                return;
            }
            let value = work();
            let wake = {
                let mut state = output.lock().expect("completion poisoned");
                if state.cancelled {
                    return;
                }
                state.value = Some(value);
                state.waker.take()
            };
            if let Some(waker) = wake {
                waker.wake();
            }
        });
        self.pool.submit(job)?;
        Ok(Blocking { state })
    }
}
struct Completion<T> {
    value: Option<T>,
    waker: Option<Waker>,
    cancelled: bool,
}
/// 后台任务结果 Future；取消可跳过未开始任务，已开始的副作用不会自动回滚。
pub struct Blocking<T> {
    state: Arc<Mutex<Completion<T>>>,
}
impl<T> Future for Blocking<T> {
    type Output = T;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<T> {
        let mut state = self.state.lock().expect("completion poisoned");
        match state.value.take() {
            Some(value) => Poll::Ready(value),
            None => {
                state.waker = Some(cx.waker().clone());
                Poll::Pending
            }
        }
    }
}
impl<T> Drop for Blocking<T> {
    fn drop(&mut self) {
        let mut state = self.state.lock().expect("completion poisoned");
        state.cancelled = true;
        state.waker = None;
    }
}
