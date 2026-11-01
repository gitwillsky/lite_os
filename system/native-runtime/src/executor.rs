//! 以 port 为 reactor 的单线程 async executor。
//!
//! # 事件来源
//!
//! 1. 任务唤醒：[`Waker`] 向 port 投递 `TASK_KEY | 任务号` 的用户 packet；每个任务至多一个待处理唤醒。
//! 2. signal 等待：[`Executor::on_signals`] 以新 key 注册 `object_wait_async`，`SIGNAL_ONE` 到达时完成。
//! 3. packet 流：[`Executor::packets`] 分配持久 key，供中断绑定、pager 等持续投递的来源使用。
//! 4. 定时器：`run` 以最早截止时间作为 `port_wait` 的 deadline，到期后唤醒对应任务。
//!
//! 其他线程可安全调用 `spawn` 与 waker；只有运行 `run`/`block_on` 的线程轮询任务。

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::Arc;
use alloc::task::Wake;
use core::future::Future;
use core::pin::Pin;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use core::task::{Context, Poll, Waker};

use native_abi::{PortPacket, Status};

use crate::sync::Mutex;
use crate::{Handle, Port};

/// 任务唤醒 packet 的 key 标志位；其余 key 属于 reactor。
const TASK_KEY: u64 = 1 << 63;

type BoxFuture = Pin<Box<dyn Future<Output = ()> + Send>>;

struct Slot {
    packets: VecDeque<PortPacket>,
    waker: Option<Waker>,
    /// 一次性等待：取出 packet 后删除 slot。
    oneshot: bool,
}

struct State {
    tasks: BTreeMap<u64, Option<BoxFuture>>,
    slots: BTreeMap<u64, Slot>,
    timers: BTreeMap<(i64, u64), Waker>,
}

/// executor。
pub struct Executor {
    port: Port,
    next_task: AtomicU64,
    next_key: AtomicU64,
    state: Mutex<State>,
}

struct TaskWaker {
    executor: &'static Executor,
    id: u64,
    // FLAG: 已有未处理的唤醒 packet；缺失时频繁唤醒会让 port 队列无界增长。
    scheduled: AtomicBool,
}

impl Wake for TaskWaker {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        if !self.scheduled.swap(true, Ordering::AcqRel) {
            // 投递失败只可能是内存耗尽；任务将在下一次被唤醒时重试，没有更好的恢复方式。
            let _ = self.executor.port.queue(TASK_KEY | self.id, 0);
        }
    }
}

impl Executor {
    /// 创建 executor（以 `'static` 生命周期存活，供 waker 引用）。
    ///
    /// # Errors
    ///
    /// port 创建失败。
    pub fn create() -> Result<&'static Self, Status> {
        let executor = Box::new(Self {
            port: Port::create()?,
            next_task: AtomicU64::new(1),
            next_key: AtomicU64::new(1),
            state: Mutex::new(State {
                tasks: BTreeMap::new(),
                slots: BTreeMap::new(),
                timers: BTreeMap::new(),
            }),
        });
        Ok(Box::leak(executor))
    }

    /// reactor port（供外部来源绑定 packet 流）。
    pub fn port(&self) -> &Port {
        &self.port
    }

    /// 加入任务。
    pub fn spawn(&'static self, future: impl Future<Output = ()> + Send + 'static) {
        let id = self.next_task.fetch_add(1, Ordering::Relaxed);
        self.state.lock().tasks.insert(id, Some(Box::pin(future)));
        // 首次轮询经同一唤醒路径进入。
        let _ = self.port.queue(TASK_KEY | id, 0);
    }

    fn poll_task(&'static self, id: u64) {
        let Some(mut future) = self.state.lock().tasks.get_mut(&id).and_then(Option::take) else {
            return;
        };
        let waker = Waker::from(Arc::new(TaskWaker {
            executor: self,
            id,
            scheduled: AtomicBool::new(false),
        }));
        let mut context = Context::from_waker(&waker);
        let done = future.as_mut().poll(&mut context).is_ready();
        let mut state = self.state.lock();
        if done {
            state.tasks.remove(&id);
        } else if let Some(slot) = state.tasks.get_mut(&id) {
            *slot = Some(future);
        }
    }

    fn dispatch(&'static self, packet: PortPacket) {
        if packet.key & TASK_KEY != 0 {
            self.poll_task(packet.key & !TASK_KEY);
            return;
        }
        let waker = {
            let mut state = self.state.lock();
            let Some(slot) = state.slots.get_mut(&packet.key) else {
                // key 已被注销（等待被取消）：丢弃迟到的 packet。
                return;
            };
            slot.packets.push_back(packet);
            slot.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    fn fire_timers(&self) {
        let now = crate::system::monotonic();
        let mut expired = alloc::vec::Vec::new();
        {
            let mut state = self.state.lock();
            while let Some(entry) = state.timers.first_entry() {
                if entry.key().0 > now {
                    break;
                }
                expired.push(entry.remove());
            }
        }
        for waker in expired {
            waker.wake();
        }
    }

    fn turn(&'static self) {
        let deadline = self
            .state
            .lock()
            .timers
            .first_key_value()
            .map_or(native_abi::DEADLINE_INFINITE, |(&(deadline, _), _)| {
                deadline
            });
        if let Ok(packet) = self.port.wait(deadline) {
            self.dispatch(packet);
        }
        self.fire_timers();
    }

    /// 持续处理事件（不返回）。
    pub fn run(&'static self) -> ! {
        loop {
            self.turn();
        }
    }

    /// 运行直到 `future` 完成，期间也推进其他任务。
    pub fn block_on<T: Send + 'static>(
        &'static self,
        future: impl Future<Output = T> + Send + 'static,
    ) -> T {
        let result = Arc::new(Mutex::new(None));
        let slot = result.clone();
        self.spawn(async move {
            *slot.lock() = Some(future.await);
        });
        loop {
            if let Some(value) = result.lock().take() {
                return value;
            }
            self.turn();
        }
    }

    fn allocate_key(&self, oneshot: bool) -> u64 {
        let key = self.next_key.fetch_add(1, Ordering::Relaxed);
        self.state.lock().slots.insert(
            key,
            Slot {
                packets: VecDeque::new(),
                waker: None,
                oneshot,
            },
        );
        key
    }

    /// 等待 `handle` 的任一 `signals` 满足，返回观察到的全部 signal。
    pub fn on_signals<'a>(&'static self, handle: &'a Handle, signals: u32) -> OnSignals<'a> {
        OnSignals {
            executor: self,
            handle,
            signals,
            key: None,
        }
    }

    /// 分配持久 packet 流：外部来源以返回的 key 向 [`Self::port`] 投递。
    pub fn packets(&'static self) -> Packets {
        Packets {
            executor: self,
            key: self.allocate_key(false),
        }
    }

    /// 睡眠到截止时间。
    pub fn sleep_until(&'static self, deadline: i64) -> Sleep {
        Sleep {
            executor: self,
            deadline,
            id: None,
        }
    }

    fn take_packet(&self, key: u64, waker: &Waker) -> Option<PortPacket> {
        let mut state = self.state.lock();
        let slot = state.slots.get_mut(&key)?;
        if let Some(packet) = slot.packets.pop_front() {
            if slot.oneshot {
                state.slots.remove(&key);
            }
            return Some(packet);
        }
        slot.waker = Some(waker.clone());
        None
    }
}

/// [`Executor::on_signals`] 的 future。
pub struct OnSignals<'a> {
    executor: &'static Executor,
    handle: &'a Handle,
    signals: u32,
    key: Option<u64>,
}

impl Future for OnSignals<'_> {
    type Output = Result<u32, Status>;

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        let key = match self.key {
            Some(key) => key,
            None => {
                let key = self.executor.allocate_key(true);
                if let Err(status) = self
                    .handle
                    .wait_async(&self.executor.port, key, self.signals)
                {
                    self.executor.state.lock().slots.remove(&key);
                    return Poll::Ready(Err(status));
                }
                self.key = Some(key);
                key
            }
        };
        match self.executor.take_packet(key, context.waker()) {
            Some(packet) => {
                self.key = None;
                Poll::Ready(if packet.status == native_abi::OK {
                    Ok(packet.observed)
                } else {
                    Err(packet.status)
                })
            }
            None => Poll::Pending,
        }
    }
}

impl Drop for OnSignals<'_> {
    fn drop(&mut self) {
        // 已注册但未完成：注销 key，迟到的 packet 由 dispatch 丢弃。
        if let Some(key) = self.key {
            self.executor.state.lock().slots.remove(&key);
        }
    }
}

/// 持久 packet 流。
pub struct Packets {
    executor: &'static Executor,
    key: u64,
}

impl Packets {
    /// 投递到本流所用的 key。
    pub fn key(&self) -> u64 {
        self.key
    }

    /// 取下一个 packet。
    pub fn next(&self) -> NextPacket<'_> {
        NextPacket { packets: self }
    }
}

impl Drop for Packets {
    fn drop(&mut self) {
        self.executor.state.lock().slots.remove(&self.key);
    }
}

/// [`Packets::next`] 的 future。
pub struct NextPacket<'a> {
    packets: &'a Packets,
}

impl Future for NextPacket<'_> {
    type Output = PortPacket;

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<PortPacket> {
        match self
            .packets
            .executor
            .take_packet(self.packets.key, context.waker())
        {
            Some(packet) => Poll::Ready(packet),
            None => Poll::Pending,
        }
    }
}

/// [`Executor::sleep_until`] 的 future。
pub struct Sleep {
    executor: &'static Executor,
    deadline: i64,
    id: Option<u64>,
}

impl Future for Sleep {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<()> {
        if crate::system::monotonic() >= self.deadline {
            if let Some(id) = self.id.take() {
                self.executor
                    .state
                    .lock()
                    .timers
                    .remove(&(self.deadline, id));
            }
            return Poll::Ready(());
        }
        let executor = self.executor;
        let id = *self
            .id
            .get_or_insert_with(|| executor.next_key.fetch_add(1, Ordering::Relaxed));
        let deadline = self.deadline;
        self.executor
            .state
            .lock()
            .timers
            .insert((deadline, id), context.waker().clone());
        Poll::Pending
    }
}

impl Drop for Sleep {
    fn drop(&mut self) {
        if let Some(id) = self.id {
            self.executor
                .state
                .lock()
                .timers
                .remove(&(self.deadline, id));
        }
    }
}
