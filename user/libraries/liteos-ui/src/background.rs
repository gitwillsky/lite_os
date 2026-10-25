//! 有界阻塞任务；同步 I/O 和计算不在 UI Future 的 poll 中执行。
use crate::{Error, Result};
use std::{
    future::Future,
    pin::Pin,
    sync::{
        Arc, Mutex,
        mpsc::{self, SyncSender, TrySendError},
    },
    task::{Context, Poll, Waker},
    thread,
};
type Job = Box<dyn FnOnce() + Send>;
const WORKERS: usize = 2;
const QUEUE: usize = 64;

/// 可在 UI Future 内提交阻塞工作的句柄；执行由应用的固定 worker 池拥有。
#[derive(Clone)]
pub struct Background {
    sender: SyncSender<Job>,
}
impl Background {
    pub(crate) fn new() -> Result<Self> {
        let (sender, receiver) = mpsc::sync_channel::<Job>(QUEUE);
        let receiver = Arc::new(Mutex::new(receiver));
        for index in 0..WORKERS {
            let receiver = receiver.clone();
            thread::Builder::new()
                .name(format!("ui-worker-{index}"))
                .spawn(move || {
                    loop {
                        // OWNER: mutex 只串行接收 channel，执行 job 时已释放，两个 worker 可并发工作。
                        // PROOF: receiver 只由这些线程使用，Sender 关闭后 recv 结束；Drop 不在 UI 线程 join。
                        // FAILURE: 失去容量限制会堆积文件/计算请求；持锁执行则所有任务退化为串行。
                        let job = receiver.lock().expect("worker queue poisoned").recv();
                        match job {
                            Ok(job) => job(),
                            Err(_) => break,
                        }
                    }
                })
                .map_err(crate::io_error)?;
        }
        Ok(Self { sender })
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
        self.sender.try_send(job).map_err(|error| Error {
            code: classic_ui_abi::FAILED,
            message: match error {
                TrySendError::Full(_) => "background queue is full",
                TrySendError::Disconnected(_) => "background workers stopped",
            }
            .into(),
        })?;
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
