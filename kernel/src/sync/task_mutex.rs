//! 可睡眠的 task 级互斥锁：FIFO 排队，释放时把所有权直接交给队首等待者。
//!
//! # 它解决什么问题
//!
//! 自旋锁的临界区内不能睡眠，也不能做可能阻塞的 I/O。但内核里有些状态必须在 I/O 期间一直被独占，例如
//! 缺页等待磁盘时的地址空间。`TaskMutex` 允许持有者在持锁期间睡眠，竞争者也是睡眠而不是自旋。它只能在
//! task 上下文里发生竞争（竞争时需要当前任务的等待目标）；中断也会访问的数据要用 `IrqMutex`，见 `sync/mod.rs`。
//!
//! # 状态机
//!
//! 内部的 `state` 是一把很短的自旋锁，只保护所有权和等待队列；**guard 不持有它**，所以持有 `TaskMutex` 时
//! 可以睡眠。所有权有三种状态：
//!
//! ```text
//! Available ──取锁──▶ Held ──释放，队列非空──▶ Handoff(ticket) ──被选中的等待者醒来取走──▶ Held
//!      ▲                 │
//!      └─释放，队列为空──┘
//! ```
//!
//! # 取锁（`lock_prepared`）
//!
//! 1. 快路径 `try_lock`：`Available → Held`，不排队，不分配。
//! 2. 慢路径：取一个预分配的 waiter，用这把锁自己递增的 ticket 作身份；在 `state` 自旋锁里再确认一次
//!    （期间持有者可能刚好释放），仍被占用就把 waiter 排到 FIFO 队尾，放开自旋锁，然后睡眠。
//! 3. 被唤醒后断言所有权已经是 `Handoff(自己的 ticket)`，改成 `Held`，返回 guard。
//!
//! # 释放：交接，而不是“放开再抢”
//!
//! 释放时在 `state` 锁内弹出队首等待者，把所有权设成 `Handoff(队首的 ticket)`——**不是** `Available`——
//! 然后在锁外唤醒它。如果设成 `Available` 再唤醒，恰好此刻到达的新调用者就能抢先拿走锁，被唤醒的等待者
//! 醒来发现锁没了，只能重新排队，先来先得的顺序无法保证。`Handoff` 让 `try_lock` 在被选中的等待者自己
//! 取走之前一直失败，新来的只能排队。唤醒放在 `state` 自旋锁之外，避免持着自旋锁进入调度器。
//!
//! # 预分配 waiter
//!
//! 排队需要一个堆分配的 waiter，`lock()` 因此可能在 OOM 时失败。对“提交尾部不能回滚”的事务，调用者先用
//! `TaskMutexWaitPreparation::prepare()` 预分配，再调用 `lock_prepared`，这一步不会再分配。同一份
//! preparation 可以依次等待多个锁。这又是“准备（可失败）与提交（不会失败）分离”。
//!
//! # 阅读顺序
//!
//! `Ownership` → `lock_prepared` → `unlock` → `TaskMutexWaitPreparation`。
mod signal_acquisition;
use alloc::sync::Arc;
use core::{
    cell::UnsafeCell,
    marker::PhantomData,
    ops::{Deref, DerefMut},
    sync::atomic::{AtomicU64, Ordering},
};
pub(crate) use signal_acquisition::TaskMutexLockError;

use super::{
    WaitCompletion,
    task_wait::{TaskWaitKey, TaskWaitMode, TaskWaitTarget, current_wait_target},
};

/// task mutex waiter metadata 分配失败。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TaskMutexOutOfMemory;

pub(super) struct Waiter {
    pub(super) key: TaskWaitKey,
    /// 读写锁队列里区分写者与读者（`TaskRwSemaphore` 释放时据此成批交接读者）；互斥锁恒为 true。
    /// 若不区分，成批唤醒读者会把写者也当成共享持有者，破坏写入互斥。
    pub(super) exclusive: bool,
    // OWNER: completion 独立保活，使 unlock 能先释放最后一个 queue/publisher waiter 引用，
    // 再发布完成。若 token 内嵌于 waiter，arming task 可在 publisher Arc 释放前恢复，
    // 导致 preparation 无法重新取得唯一 waiter ownership。
    completion: Arc<WaitCompletion>,
    target: Option<Arc<dyn TaskWaitTarget>>,
    pub(super) next: spin::Mutex<Option<Arc<Waiter>>>,
}

impl Waiter {
    fn allocate() -> Result<Arc<Self>, TaskMutexOutOfMemory> {
        let completion = Arc::try_new(WaitCompletion::new()).map_err(|_| TaskMutexOutOfMemory)?;
        Arc::try_new(Self {
            key: TaskWaitKey {
                owner: 0,
                ticket: 0,
                deadline_ns: None,
                mode: TaskWaitMode::Uninterruptible,
            },
            exclusive: true,
            completion,
            target: None,
            next: spin::Mutex::new(None),
        })
        .map_err(|_| TaskMutexOutOfMemory)
    }

    pub(super) fn wait(self: &Arc<Self>) {
        let result = self
            .target
            .as_ref()
            .cloned()
            .expect("task mutex waiter target disappeared before sleep")
            .sleep(self.completion.as_ref(), self.key);
        assert_eq!(result, super::WaitResult::Woken);
    }

    fn retire(self: Arc<Self>) -> (Arc<WaitCompletion>, Wake) {
        let completion = self.completion.clone();
        let wake = Wake {
            target: self
                .target
                .as_ref()
                .cloned()
                .expect("task mutex waiter target disappeared before wake"),
            key: self.key,
        };
        drop(self);
        (completion, wake)
    }

    pub(super) fn publish(self: Arc<Self>) -> Option<Wake> {
        let (completion, wake) = self.retire();
        completion.complete().then_some(wake)
    }
}

/// 在不可回滚 transaction 前预分配的一份 task-mutex waiter metadata。
///
/// 同一 preparation 可依次等待多个 mutex；每次 guard 成功取得后 queue 不再保留 waiter
/// 引用，下一次 arm 才会覆写 identity。缺少该 preflight 时，post-commit invalidation
/// 可能因 waiter OOM 无法完成，留下 hardware 可见的 stale state。
pub(crate) struct TaskMutexWaitPreparation {
    waiter: Arc<Waiter>,
}

impl core::fmt::Debug for TaskMutexWaitPreparation {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("TaskMutexWaitPreparation")
            .finish_non_exhaustive()
    }
}

impl TaskMutexWaitPreparation {
    /// 为当前 task 预分配可复用 waiter metadata。
    ///
    /// # Errors
    ///
    /// control block 分配失败返回 OutOfMemory。
    pub(crate) fn prepare() -> Result<Self, TaskMutexOutOfMemory> {
        Ok(Self {
            waiter: Waiter::allocate()?,
        })
    }

    fn arm<T: ?Sized>(&mut self, mutex: &TaskMutex<T>) -> Arc<Waiter> {
        self.arm_mode(mutex, TaskWaitMode::Uninterruptible)
    }
    fn arm_mode<T: ?Sized>(&mut self, mutex: &TaskMutex<T>, mode: TaskWaitMode) -> Arc<Waiter> {
        self.arm_for(
            core::ptr::from_ref(mutex).cast::<()>() as usize,
            &mutex.next_ticket,
            mode,
            true,
        )
    }
    /// 为 task mutex 或读写信号量的本次等待装填预分配 waiter。
    ///
    /// # Parameters
    /// - `owner`：锁的稳定地址，用于 scheduler 的精确等待身份。
    /// - `tickets`：该锁自己的 ticket 序列，不承载受保护数据的 memory ordering。
    /// - `mode`：本次等待的 signal policy。
    /// - `exclusive`：互斥/写者为 true，读写信号量的共享读者为 false。
    ///
    /// # Returns
    /// 可移交队列的 waiter 引用；取得锁后由同一 preparation disarm。
    ///
    /// # Panics
    /// preparation 尚有前次引用、ticket 耗尽或在非 task 上下文竞争时 fail-stop。
    pub(super) fn arm_for(
        &mut self,
        owner: usize,
        tickets: &AtomicU64,
        mode: TaskWaitMode,
        exclusive: bool,
    ) -> Arc<Waiter> {
        let waiter = Arc::get_mut(&mut self.waiter)
            .expect("task mutex wait preparation reused before acquisition");
        let ticket = tickets
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                current.checked_add(1)
            })
            .expect("task mutex waiter ticket exhausted");
        waiter.key = TaskWaitKey {
            owner,
            ticket,
            deadline_ns: None,
            mode,
        };
        waiter.exclusive = exclusive;
        waiter.target =
            Some(current_wait_target().expect("contended task mutex outside task context"));
        waiter.completion.reset();
        waiter.next = spin::Mutex::new(None);
        self.waiter.clone()
    }

    pub(super) fn disarm(&mut self, waiter: Arc<Waiter>) {
        waiter.completion.complete();
        drop(waiter);
        Arc::get_mut(&mut self.waiter)
            .expect("task mutex waiter retained after acquisition")
            .target = None;
    }
}

pub(super) struct Wake {
    target: Arc<dyn TaskWaitTarget>,
    key: TaskWaitKey,
}

impl Wake {
    pub(super) fn wake(self) {
        self.target.wake(self.key);
    }
}

struct LockState {
    ownership: Ownership,
    head: Option<Arc<Waiter>>,
    tail: Option<Arc<Waiter>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ownership {
    Available,
    Held,
    Handoff(u64),
}

impl LockState {
    const fn new() -> Self {
        Self {
            ownership: Ownership::Available,
            head: None,
            tail: None,
        }
    }

    fn push(&mut self, waiter: Arc<Waiter>) {
        if let Some(tail) = self.tail.replace(waiter.clone()) {
            assert!(tail.next.lock().replace(waiter).is_none());
        } else {
            assert!(self.head.replace(waiter).is_none());
        }
    }

    fn pop(&mut self) -> Option<Arc<Waiter>> {
        let waiter = self.head.take()?;
        self.head = waiter.next.lock().take();
        if self.head.is_none() {
            self.tail = None;
        }
        Some(waiter)
    }
}

/// 可跨调度和可睡眠 I/O 保活的 task-context mutex。
///
/// state spin lock 只保护 owner bit 与预分配 waiter 链；guard 不保留该 spin lock。竞争
/// task 发布精确 scheduler membership 后进入 Blocked，unlock 摘取一个 FIFO waiter 并在
/// state lock 外唤醒。`Handoff(ticket)` 禁止新 caller 越过已选择 waiter。
pub(crate) struct TaskMutex<T: ?Sized> {
    state: spin::Mutex<LockState>,
    next_ticket: AtomicU64,
    value: UnsafeCell<T>,
}

impl<T> TaskMutex<T> {
    /// 创建未锁定的 task mutex。
    pub(crate) const fn new(value: T) -> Self {
        Self {
            state: spin::Mutex::new(LockState::new()),
            next_ticket: AtomicU64::new(1),
            value: UnsafeCell::new(value),
        }
    }
}

impl<T: ?Sized> TaskMutex<T> {
    /// 阻塞取得 task-context guard。
    ///
    /// # Returns
    ///
    /// 独占 guard。
    ///
    /// # Errors
    ///
    /// waiter metadata 分配失败时返回 `TaskMutexOutOfMemory`。
    pub(crate) fn lock(&self) -> Result<TaskMutexGuard<'_, T>, TaskMutexOutOfMemory> {
        if let Some(guard) = self.try_lock() {
            return Ok(guard);
        }
        let mut preparation = TaskMutexWaitPreparation::prepare()?;
        Ok(self.lock_prepared(&mut preparation))
    }

    /// 使用 caller 已预分配的 waiter metadata 阻塞取得 guard。
    ///
    /// # Parameters
    ///
    /// - `preparation`: 当前 task 独占、且前一次 acquisition 已完成的 preparation。
    ///
    /// # Returns
    ///
    /// 独占 guard；本阶段不再分配，因此适用于不可回滚 transaction 的提交尾部。
    pub(crate) fn lock_prepared(
        &self,
        preparation: &mut TaskMutexWaitPreparation,
    ) -> TaskMutexGuard<'_, T> {
        if let Some(guard) = self.try_lock() {
            return guard;
        }
        let waiter = preparation.arm(self);
        {
            let mut state = self.state.lock();
            if state.ownership == Ownership::Available {
                state.ownership = Ownership::Held;
                drop(state);
                preparation.disarm(waiter);
                return TaskMutexGuard::new(self);
            }
            state.push(waiter.clone());
        }
        waiter.wait();
        let mut state = self.state.lock();
        assert_eq!(
            state.ownership,
            Ownership::Handoff(waiter.key.ticket),
            "task mutex waiter woke without its handoff"
        );
        state.ownership = Ownership::Held;
        drop(state);
        preparation.disarm(waiter);
        TaskMutexGuard::new(self)
    }

    /// 仅在当前无 owner 时取得 guard，不排队、不分配。
    pub(crate) fn try_lock(&self) -> Option<TaskMutexGuard<'_, T>> {
        let mut state = self.state.lock();
        if state.ownership != Ownership::Available {
            return None;
        }
        state.ownership = Ownership::Held;
        Some(TaskMutexGuard::new(self))
    }

    fn unlock(&self) -> Option<Wake> {
        let waiter = {
            let mut state = self.state.lock();
            assert_eq!(
                state.ownership,
                Ownership::Held,
                "task mutex unlocked without owner"
            );
            let waiter = state.pop();
            state.ownership = waiter.as_ref().map_or(Ownership::Available, |waiter| {
                Ownership::Handoff(waiter.key.ticket)
            });
            waiter
        };
        waiter.and_then(Waiter::publish)
    }
}

// SAFETY: LockState serializes ownership and guard construction; T: Send permits the owning task
// stack to resume on another scheduler CPU before releasing the guard.
unsafe impl<T: ?Sized + Send> Send for TaskMutex<T> {}
// SAFETY: only one live guard can access value, published through the state lock handoff.
unsafe impl<T: ?Sized + Send> Sync for TaskMutex<T> {}

/// `TaskMutex` 的 task-context 独占 guard。
#[must_use = "dropping the guard releases the task mutex"]
pub(crate) struct TaskMutexGuard<'a, T: ?Sized> {
    mutex: &'a TaskMutex<T>,
    _not_send: PhantomData<*mut ()>,
}

impl<'a, T: ?Sized> TaskMutexGuard<'a, T> {
    fn new(mutex: &'a TaskMutex<T>) -> Self {
        Self {
            mutex,
            _not_send: PhantomData,
        }
    }
}

impl<T: ?Sized> Deref for TaskMutexGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        // SAFETY: this guard is the unique owner published by LockState.locked.
        unsafe { &*self.mutex.value.get() }
    }
}

impl<T: ?Sized> DerefMut for TaskMutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        // SAFETY: the unique live guard has exclusive access until Drop.
        unsafe { &mut *self.mutex.value.get() }
    }
}

impl<T: ?Sized> Drop for TaskMutexGuard<'_, T> {
    fn drop(&mut self) {
        if let Some(wake) = self.mutex.unlock() {
            wake.wake();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{TaskMutex, TaskMutexWaitPreparation};
    use alloc::sync::Arc;
    use std::{
        sync::mpsc::{self, TryRecvError},
        thread,
        time::Duration,
    };

    fn queued(mutex: &TaskMutex<usize>) -> usize {
        let state = mutex.state.lock();
        let mut count = 0;
        let mut cursor = state.head.clone();
        while let Some(waiter) = cursor {
            count += 1;
            cursor = waiter.next.lock().clone();
        }
        count
    }

    fn wait_until_queued(mutex: &TaskMutex<usize>, expected: usize) {
        for _ in 0..1_000 {
            if queued(mutex) == expected {
                return;
            }
            thread::sleep(Duration::from_millis(1));
        }
        panic!("task mutex did not publish {expected} waiters");
    }

    #[test]
    fn contention_blocks_and_handoffs_in_fifo_order() {
        let mutex = Arc::new(TaskMutex::new(0_usize));
        let owner = mutex.lock().expect("initial task mutex lock");
        let (acquired_tx, acquired_rx) = mpsc::channel();
        let (first_release_tx, first_release_rx) = mpsc::channel();

        let first_mutex = mutex.clone();
        let first_tx = acquired_tx.clone();
        let first = thread::spawn(move || {
            let mut guard = first_mutex.lock().expect("first waiter lock");
            *guard = 1;
            first_tx.send(1).expect("report first acquisition");
            first_release_rx.recv().expect("release first waiter");
        });
        wait_until_queued(&mutex, 1);

        let second_mutex = mutex.clone();
        let second = thread::spawn(move || {
            let mut guard = second_mutex.lock().expect("second waiter lock");
            assert_eq!(*guard, 1);
            *guard = 2;
            acquired_tx.send(2).expect("report second acquisition");
        });
        wait_until_queued(&mutex, 2);

        drop(owner);
        assert_eq!(acquired_rx.recv().expect("first acquisition"), 1);
        assert!(matches!(acquired_rx.try_recv(), Err(TryRecvError::Empty)));
        assert!(mutex.try_lock().is_none(), "caller bypassed handoff owner");
        first_release_tx.send(()).expect("release first owner");
        assert_eq!(acquired_rx.recv().expect("second acquisition"), 2);

        first.join().expect("first waiter panicked");
        second.join().expect("second waiter panicked");
        assert_eq!(*mutex.try_lock().expect("released mutex remained busy"), 2);
    }

    #[test]
    fn prepared_waiter_is_pointer_sized_and_reusable_after_handoff() {
        assert_eq!(
            core::mem::size_of::<TaskMutexWaitPreparation>(),
            core::mem::size_of::<Arc<()>>()
        );
        let first = Arc::new(TaskMutex::new(0_usize));
        let second = Arc::new(TaskMutex::new(0_usize));
        let first_owner = first.lock().expect("first owner");
        let second_owner = second.lock().expect("second owner");
        let (acquired_tx, acquired_rx) = mpsc::channel();
        let (continue_tx, continue_rx) = mpsc::channel();
        let waiter_first = first.clone();
        let waiter_second = second.clone();
        let waiter = thread::spawn(move || {
            let mut preparation = TaskMutexWaitPreparation::prepare().expect("wait preparation");
            let first_guard = waiter_first.lock_prepared(&mut preparation);
            acquired_tx.send(1).expect("first prepared acquisition");
            continue_rx.recv().expect("continue prepared waiter");
            drop(first_guard);
            let _second_guard = waiter_second.lock_prepared(&mut preparation);
            acquired_tx.send(2).expect("second prepared acquisition");
        });

        wait_until_queued(&first, 1);
        drop(first_owner);
        assert_eq!(acquired_rx.recv().expect("first result"), 1);
        continue_tx.send(()).expect("continue waiter");
        wait_until_queued(&second, 1);
        drop(second_owner);
        assert_eq!(acquired_rx.recv().expect("second result"), 2);
        waiter.join().expect("prepared waiter panicked");
    }

    #[test]
    fn publication_window_retires_queue_owner_before_completion() {
        let mutex = TaskMutex::new(());
        let mut preparation = TaskMutexWaitPreparation::prepare().expect("wait preparation");
        let waiter = preparation.arm(&mutex);
        assert!(waiter.completion.begin_arming());

        let (completion, wake) = waiter.clone().retire();
        assert_eq!(
            Arc::strong_count(&waiter),
            2,
            "publisher retained the waiter visible to the acquiring task"
        );
        assert!(!completion.complete());
        assert!(waiter.completion.finish_arming());
        drop(wake);
        preparation.disarm(waiter);
    }
}
