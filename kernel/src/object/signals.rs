//! 对象 signal 与 observer：同步等待与 port 异步等待的唯一触发机制。
//!
//! 1. 每个可等待对象拥有一个 [`SignalState`]，保存当前 signal 与已登记 observer。
//! 2. signal 变化时在同一临界区找出被满足的 observer，摘除并立即通知；observer 是一次性的。
//! 3. 通知不分配内存：同步等待只置位 `TaskEvent`，port 投递使用登记时预留的队列空位。
//! 4. 句柄关闭按 [`CancelKey`] 取消经该句柄登记的 observer，同步等待者收到 `ERR_CANCELED`。

use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use native_abi::Status;
use spin::Mutex;

use super::port::Port;
use crate::execution::sync::lock_with_spare;
use crate::execution::sync::{TaskEvent, WaitResult};

/// 标识 observer 来源的句柄；同一句柄关闭时取消其全部登记。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CancelKey {
    /// 句柄表的全局唯一编号。
    pub(crate) table: u64,
    /// 句柄值。
    pub(crate) handle: u32,
}

/// 一个线程的一次同步等待。
pub(crate) struct WaitObserver {
    event: TaskEvent,
    // OWNER: 触发时刻的 signal 快照；在 event signal 之前发布，等待者被唤醒后读取。
    observed: AtomicU32,
    // FLAG: 句柄关闭取消了这次等待；缺失时等待者无法区分 signal 满足与取消，会把已关闭的句柄当成就绪。
    canceled: AtomicBool,
}

/// 同步等待的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WaitOutcome {
    /// 满足的 signal 快照。
    Satisfied(u32),
    /// 截止时间到达；附当前 signal。
    TimedOut(u32),
    /// 句柄被关闭。
    Canceled,
    /// 所在进程正在终止。
    Interrupted,
}

enum ObserverTarget {
    Wait(Arc<WaitObserver>),
    Port { port: Weak<Port>, key: u64 },
}

struct Observer {
    trigger: u32,
    cancel: CancelKey,
    target: ObserverTarget,
}

struct SignalInner {
    active: u32,
    observers: Vec<Observer>,
}

/// 对象的 signal 与 observer 集合。
pub(crate) struct SignalState {
    // OWNER: signal 位与 observer 列表必须同锁发布；拆开会让登记与触发之间的变化丢失唤醒。
    inner: Mutex<SignalInner>,
}

impl SignalState {
    /// 以初始 signal 构造。
    pub(crate) const fn new(active: u32) -> Self {
        Self {
            inner: Mutex::new(SignalInner {
                active,
                observers: Vec::new(),
            }),
        }
    }

    /// 当前 signal。
    pub(crate) fn active(&self) -> u32 {
        self.inner.lock().active
    }

    /// 清除再置位 signal，并通知被满足的 observer。
    pub(crate) fn update(&self, clear: u32, set: u32) {
        let mut inner = self.inner.lock();
        let active = (inner.active & !clear) | set;
        if active == inner.active {
            return;
        }
        inner.active = active;
        inner.observers.retain(|observer| {
            if observer.trigger & active == 0 {
                return true;
            }
            notify(&observer.target, observer.trigger, active);
            false
        });
    }

    /// 取消经 `key` 登记的全部 observer。
    pub(crate) fn cancel(&self, key: CancelKey) {
        self.inner.lock().observers.retain(|observer| {
            if observer.cancel != key {
                return true;
            }
            match &observer.target {
                ObserverTarget::Wait(wait) => {
                    wait.canceled.store(true, Ordering::Release);
                    wait.event.signal();
                }
                ObserverTarget::Port { port, .. } => {
                    if let Some(port) = port.upgrade() {
                        port.release_reservation();
                    }
                }
            }
            false
        });
    }

    /// 阻塞当前线程，直到 `trigger` 中任一 signal 满足、截止时间到达、句柄关闭或进程终止。
    ///
    /// # Parameters
    /// - `trigger`: 等待的 signal 掩码。
    /// - `cancel`: 等待所经句柄。
    /// - `deadline_ns`: 单调时钟绝对截止时间；`None` 表示不超时。
    ///
    /// # Errors
    /// observer 分配失败返回 `ERR_NO_MEMORY`。
    pub(crate) fn wait(
        &self,
        trigger: u32,
        cancel: CancelKey,
        deadline_ns: Option<u64>,
    ) -> Result<WaitOutcome, Status> {
        let observer = Arc::try_new(WaitObserver {
            event: TaskEvent::new(),
            observed: AtomicU32::new(0),
            canceled: AtomicBool::new(false),
        })
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        {
            let mut inner = lock_with_spare(&self.inner, |_| 1, |inner| &mut inner.observers)
                .map_err(|()| native_abi::ERR_NO_MEMORY)?;
            if inner.active & trigger != 0 {
                return Ok(WaitOutcome::Satisfied(inner.active));
            }
            if deadline_ns
                .is_some_and(|deadline| deadline <= crate::execution::timer::get_time_ns())
            {
                return Ok(WaitOutcome::TimedOut(inner.active));
            }
            inner.observers.push(Observer {
                trigger,
                cancel,
                target: ObserverTarget::Wait(observer.clone()),
            });
        }
        let result = match deadline_ns {
            Some(deadline) => observer.event.wait_interruptible_until(deadline),
            None => observer.event.wait_interruptible(),
        };
        // 未被触发时 observer 仍在列表中；摘除后再判定，避免迟到的触发与超时同时生效。
        let active = {
            let mut inner = self.inner.lock();
            inner.observers.retain(|entry| {
                !matches!(&entry.target, ObserverTarget::Wait(wait) if Arc::ptr_eq(wait, &observer))
            });
            inner.active
        };
        if observer.canceled.load(Ordering::Acquire) {
            return Ok(WaitOutcome::Canceled);
        }
        let observed = observer.observed.load(Ordering::Acquire);
        if observed != 0 {
            return Ok(WaitOutcome::Satisfied(observed));
        }
        Ok(match result {
            WaitResult::TimedOut => WaitOutcome::TimedOut(active),
            WaitResult::OutOfMemory => return Err(native_abi::ERR_NO_MEMORY),
            WaitResult::Interrupted | WaitResult::Woken => WaitOutcome::Interrupted,
        })
    }

    /// 登记一次性 port 投递；signal 已满足时立即投递。
    ///
    /// # Errors
    /// port 队列或 observer 预留失败返回 `ERR_NO_MEMORY`。
    pub(crate) fn wait_async(
        &self,
        trigger: u32,
        cancel: CancelKey,
        port: &Arc<Port>,
        key: u64,
    ) -> Result<(), Status> {
        port.reserve()?;
        let Ok(mut inner) = lock_with_spare(&self.inner, |_| 1, |inner| &mut inner.observers)
        else {
            port.release_reservation();
            return Err(native_abi::ERR_NO_MEMORY);
        };
        let target = ObserverTarget::Port {
            port: Arc::downgrade(port),
            key,
        };
        if inner.active & trigger != 0 {
            notify(&target, trigger, inner.active);
            return Ok(());
        }
        inner.observers.push(Observer {
            trigger,
            cancel,
            target,
        });
        Ok(())
    }
}

impl Drop for SignalState {
    fn drop(&mut self) {
        // 对象销毁时 port 登记永不触发，归还预留空位；同步等待者必然持有对象引用，不可能仍在列表中。
        for observer in self.inner.get_mut().observers.drain(..) {
            if let ObserverTarget::Port { port, .. } = observer.target
                && let Some(port) = port.upgrade()
            {
                port.release_reservation();
            }
        }
    }
}

fn notify(target: &ObserverTarget, trigger: u32, active: u32) {
    match target {
        ObserverTarget::Wait(wait) => {
            wait.observed.store(active, Ordering::Release);
            wait.event.signal();
        }
        ObserverTarget::Port { port, key } => {
            if let Some(port) = port.upgrade() {
                port.deliver(native_abi::PortPacket {
                    key: *key,
                    kind: native_abi::packet::SIGNAL_ONE,
                    status: native_abi::OK,
                    trigger,
                    observed: active,
                    payload: 0,
                });
            }
        }
    }
}
