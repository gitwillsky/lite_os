//! 单线程 Future 调度；跨线程 Waker 只登记就绪，不访问任何 UI 对象。
use crate::{Error, Result};
use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, VecDeque},
    future::Future,
    io::{self, Read, Write},
    os::unix::net::UnixStream,
    pin::Pin,
    rc::Rc,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll, Wake, Waker},
    time::{Duration, Instant},
};

pub(crate) const POLL_BUDGET: usize = 64;
const TASK_LIMIT: usize = 4096;
pub(crate) struct Notifications {
    queue: Mutex<VecDeque<u64>>,
    socket: UnixStream,
}
impl Notifications {
    pub fn pair() -> io::Result<(Arc<Self>, UnixStream)> {
        let (reader, socket) = UnixStream::pair()?;
        reader.set_nonblocking(true)?;
        socket.set_nonblocking(true)?;
        Ok((
            Arc::new(Self {
                queue: Mutex::new(VecDeque::new()),
                socket,
            }),
            reader,
        ))
    }
    fn signal(&self) {
        let mut socket = &self.socket;
        loop {
            match socket.write(&[1]) {
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                _ => break,
            }
        }
    }
    pub fn drain(reader: &mut UnixStream) -> io::Result<()> {
        let mut bytes = [0; 256];
        loop {
            match reader.read(&mut bytes) {
                Ok(0) => return Ok(()),
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
    }
    pub fn ready(&self) -> bool {
        !self.queue.lock().expect("ready queue poisoned").is_empty()
    }
}
struct TaskWake {
    id: u64,
    notify: Arc<Notifications>,
    active: AtomicBool,
    queued: AtomicBool,
    cancelled: AtomicBool,
}
impl TaskWake {
    fn enqueue(&self) {
        if self.active.load(Ordering::Acquire) && !self.queued.swap(true, Ordering::AcqRel) {
            // OWNER: mutex 只保护就绪编号，绝不在锁内 poll Future 或执行应用代码。
            // PROOF: queued 合并重复唤醒；先入队再写非阻塞 socket，满 socket 已有未消费唤醒。
            // FAILURE: 缺失排队或信号会丢失跨线程完成；失去合并会使队列无界增长。
            self.notify
                .queue
                .lock()
                .expect("ready queue poisoned")
                .push_back(self.id);
            self.notify.signal();
        }
    }
}
impl Wake for TaskWake {
    fn wake(self: Arc<Self>) {
        self.enqueue();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.enqueue();
    }
}

pub(crate) struct Scope {
    pub closed: Cell<bool>,
    tasks: RefCell<Vec<Weak<TaskWake>>>,
    observers: RefCell<Vec<std::rc::Weak<dyn OnCancel>>>,
}
pub(crate) trait OnCancel {
    fn cancel(&self);
}
impl OnCancel for Scope {
    fn cancel(&self) {
        Scope::cancel(self);
    }
}
impl Scope {
    pub fn new() -> Self {
        Self {
            closed: Cell::new(false),
            tasks: RefCell::new(Vec::new()),
            observers: RefCell::new(Vec::new()),
        }
    }
    pub fn cancel(&self) {
        if !self.closed.replace(true) {
            for task in self.tasks.borrow().iter().filter_map(Weak::upgrade) {
                task.cancelled.store(true, Ordering::Release);
                task.enqueue();
            }
            let observers = std::mem::take(&mut *self.observers.borrow_mut());
            for observer in observers
                .into_iter()
                .filter_map(|observer| observer.upgrade())
            {
                observer.cancel();
            }
        }
    }
    pub fn observe(&self, observer: Rc<dyn OnCancel>) {
        if self.closed.get() {
            observer.cancel();
            return;
        }
        let mut observers = self.observers.borrow_mut();
        observers.retain(|observer| observer.strong_count() != 0);
        observers.push(Rc::downgrade(&observer));
    }
    fn register(&self, wake: &Arc<TaskWake>) {
        let mut tasks = self.tasks.borrow_mut();
        tasks.retain(|task| {
            task.upgrade()
                .is_some_and(|task| task.active.load(Ordering::Acquire))
        });
        tasks.push(Arc::downgrade(wake));
    }
}
/// 请求取消后的异步任务结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cancelled;
impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("task cancelled")
    }
}
impl std::error::Error for Cancelled {}
struct Completion<T> {
    value: Option<std::result::Result<T, Cancelled>>,
    finished: bool,
    waker: Option<Waker>,
}
struct Complete<T> {
    state: Rc<RefCell<Completion<T>>>,
}
impl<T> Complete<T> {
    fn finish(self, value: T) {
        let wake = {
            let mut state = self.state.borrow_mut();
            state.value = Some(Ok(value));
            state.finished = true;
            state.waker.take()
        };
        if let Some(waker) = wake {
            waker.wake();
        }
    }
}
impl<T> Drop for Complete<T> {
    fn drop(&mut self) {
        let wake = {
            let mut state = self.state.borrow_mut();
            if state.finished {
                return;
            }
            state.finished = true;
            state.value = Some(Err(Cancelled));
            state.waker.take()
        };
        if let Some(waker) = wake {
            waker.wake();
        }
    }
}
/// 可跨线程的取消通知句柄；只写原子状态和唤醒 socket，不访问 UI。
#[derive(Clone)]
pub struct AbortHandle {
    wake: Arc<TaskWake>,
}
impl AbortHandle {
    /// 请求取消，资源在 UI 线程后续分发中释放。
    pub fn abort(&self) {
        self.wake.cancelled.store(true, Ordering::Release);
        self.wake.enqueue();
    }
}
/// 类型化任务结果 Future；Drop 取消任务，不阻塞等待，取消结果为 Cancelled。
pub struct Task<T = ()> {
    wake: Arc<TaskWake>,
    completion: Rc<RefCell<Completion<T>>>,
}
impl<T> Task<T> {
    /// 请求取消；单次正在执行的 poll 仍需自行快速返回。
    pub fn cancel(&self) {
        self.abort_handle().abort();
    }
    /// 获取可发送到后台线程的取消句柄。
    pub fn abort_handle(&self) -> AbortHandle {
        AbortHandle {
            wake: self.wake.clone(),
        }
    }
    /// Future 已完成或已在 UI 线程释放时返回 true。
    pub fn is_finished(&self) -> bool {
        self.completion.borrow().finished
    }
}
impl<T> Future for Task<T> {
    type Output = std::result::Result<T, Cancelled>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut completion = self.completion.borrow_mut();
        if let Some(value) = completion.value.take() {
            Poll::Ready(value)
        } else if completion.finished {
            panic!("Task polled after completion")
        } else {
            completion.waker = Some(cx.waker().clone());
            Poll::Pending
        }
    }
}
impl<T> Drop for Task<T> {
    fn drop(&mut self) {
        self.cancel();
    }
}
struct Entry {
    future: Pin<Box<dyn Future<Output = ()>>>,
    wake: Arc<TaskWake>,
    scope: Rc<Scope>,
}
pub(crate) struct Executor {
    tasks: BTreeMap<u64, Entry>,
    next: u64,
    notify: Arc<Notifications>,
}
impl Executor {
    pub fn new(notify: Arc<Notifications>) -> Self {
        Self {
            tasks: BTreeMap::new(),
            next: 1,
            notify,
        }
    }
    pub fn spawn<T: 'static>(
        &mut self,
        scope: Rc<Scope>,
        future: impl Future<Output = T> + 'static,
    ) -> Result<Task<T>> {
        if scope.closed.get() || self.tasks.len() >= TASK_LIMIT {
            return Err(Error {
                code: classic_ui_abi::INVALID,
                message: "task scope closed or task limit reached".into(),
            });
        }
        let id = self.next;
        self.next = self.next.checked_add(1).ok_or_else(|| Error {
            code: classic_ui_abi::FAILED,
            message: "task identities exhausted".into(),
        })?;
        let wake = Arc::new(TaskWake {
            id,
            notify: self.notify.clone(),
            active: AtomicBool::new(true),
            queued: AtomicBool::new(false),
            cancelled: AtomicBool::new(false),
        });
        scope.register(&wake);
        let completion = Rc::new(RefCell::new(Completion {
            value: None,
            finished: false,
            waker: None,
        }));
        let complete = Complete {
            state: completion.clone(),
        };
        let future = async move {
            let value = future.await;
            complete.finish(value);
        };
        self.tasks.insert(
            id,
            Entry {
                future: Box::pin(future),
                wake: wake.clone(),
                scope,
            },
        );
        wake.enqueue();
        Ok(Task { wake, completion })
    }
    pub fn poll(&mut self) {
        for _ in 0..POLL_BUDGET {
            let id = self
                .notify
                .queue
                .lock()
                .expect("ready queue poisoned")
                .pop_front();
            let Some(id) = id else {
                break;
            };
            let Some(mut entry) = self.tasks.remove(&id) else {
                continue;
            };
            entry.wake.queued.store(false, Ordering::Release);
            if entry.scope.closed.get() || entry.wake.cancelled.load(Ordering::Acquire) {
                entry.wake.active.store(false, Ordering::Release);
                continue;
            }
            let waker = Waker::from(entry.wake.clone());
            let mut context = Context::from_waker(&waker);
            if entry.future.as_mut().poll(&mut context).is_ready() {
                entry.wake.active.store(false, Ordering::Release);
            } else {
                self.tasks.insert(id, entry);
            }
        }
    }
    pub fn clear(&mut self) {
        for task in self.tasks.values() {
            task.wake.active.store(false, Ordering::Release);
        }
        self.tasks.clear();
        self.notify
            .queue
            .lock()
            .expect("ready queue poisoned")
            .clear();
    }
}
impl Drop for Executor {
    fn drop(&mut self) {
        self.clear();
    }
}

pub(crate) struct Timers {
    next: Cell<u64>,
    entries: RefCell<BTreeMap<u64, (Instant, Waker)>>,
}
impl Timers {
    pub fn new() -> Rc<Self> {
        Rc::new(Self {
            next: Cell::new(1),
            entries: RefCell::new(BTreeMap::new()),
        })
    }
    pub fn next(&self) -> Option<Duration> {
        self.entries
            .borrow()
            .values()
            .map(|(deadline, _)| deadline.saturating_duration_since(Instant::now()))
            .min()
    }
    pub fn fire(&self) {
        let now = Instant::now();
        let ready: Vec<_> = self
            .entries
            .borrow()
            .iter()
            .filter_map(|(id, (at, _))| (*at <= now).then_some(*id))
            .collect();
        let wakers: Vec<_> = {
            let mut entries = self.entries.borrow_mut();
            ready
                .into_iter()
                .filter_map(|id| entries.remove(&id).map(|(_, w)| w))
                .collect()
        };
        for waker in wakers {
            waker.wake();
        }
    }
    pub fn sleep(self: &Rc<Self>, duration: Duration) -> Result<Sleep> {
        let deadline = Instant::now().checked_add(duration).ok_or_else(|| Error {
            code: classic_ui_abi::INVALID,
            message: "timer deadline overflow".into(),
        })?;
        let id = self.next.get();
        self.next.set(id.checked_add(1).ok_or_else(|| Error {
            code: classic_ui_abi::FAILED,
            message: "timer identities exhausted".into(),
        })?);
        Ok(Sleep {
            timers: self.clone(),
            deadline,
            id,
        })
    }
}
/// 非阻塞定时 Future；Drop 撤销登记，不遗留后台线程或超时回调。
pub struct Sleep {
    timers: Rc<Timers>,
    deadline: Instant,
    id: u64,
}
impl Future for Sleep {
    type Output = ();
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if Instant::now() >= self.deadline {
            self.timers.entries.borrow_mut().remove(&self.id);
            Poll::Ready(())
        } else {
            self.timers
                .entries
                .borrow_mut()
                .insert(self.id, (self.deadline, cx.waker().clone()));
            Poll::Pending
        }
    }
}
impl Drop for Sleep {
    fn drop(&mut self) {
        self.timers.entries.borrow_mut().remove(&self.id);
    }
}

/// 可复制的单调时钟入口；只创建 Future，不产生线程或独立事件循环。
#[derive(Clone)]
pub struct Clock {
    pub(crate) timers: Rc<Timers>,
}
impl Clock {
    /// 等待 duration；期限溢出返回错误，丢弃 Future 取消等待。
    pub fn sleep(&self, duration: Duration) -> Result<Sleep> {
        self.timers.sleep(duration)
    }
}
