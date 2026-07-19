use alloc::sync::Arc;
use core::{
    cell::UnsafeCell,
    marker::PhantomData,
    ops::{Deref, DerefMut},
    sync::atomic::{AtomicU64, Ordering},
};

use super::WaitCompletion;

/// @description task mutex waiter 的精确 scheduler membership identity。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TaskMutexWaitKey {
    owner: usize,
    ticket: u64,
}

/// @description scheduler 为 task-context mutex waiter 提供的 opaque target。
pub(crate) trait TaskMutexWaitTarget: Send + Sync {
    /// @description 原子发布 membership，并在 unlock 尚未发生时阻塞当前 task。
    fn sleep(self: Arc<Self>, completion: &WaitCompletion, key: TaskMutexWaitKey);

    /// @description 消费精确 membership 并使 blocked task 可运行。
    fn wake(self: Arc<Self>, key: TaskMutexWaitKey);
}

type WaitTargetFactory = fn() -> Option<Arc<dyn TaskMutexWaitTarget>>;

// OWNER: task topology 初始化后只安装一次 scheduler adapter；缺失时启动期竞争必须
// fail-stop，不能退回 spin/yield polling。
static WAIT_TARGET_FACTORY: spin::Once<WaitTargetFactory> = spin::Once::new();

/// @description 安装 task mutex 唯一 scheduler adapter。
#[cfg_attr(test, allow(dead_code))]
pub(crate) fn install_wait_target_factory(factory: WaitTargetFactory) {
    assert!(
        WAIT_TARGET_FACTORY.get().is_none(),
        "task mutex wait factory installed twice"
    );
    WAIT_TARGET_FACTORY.call_once(|| factory);
}

fn current_wait_target() -> Option<Arc<dyn TaskMutexWaitTarget>> {
    if let Some(target) = WAIT_TARGET_FACTORY.get().and_then(|factory| factory()) {
        return Some(target);
    }
    #[cfg(test)]
    {
        Some(Arc::new(TestThreadTarget(std::thread::current())))
    }
    #[cfg(not(test))]
    None
}

#[cfg(test)]
struct TestThreadTarget(std::thread::Thread);

#[cfg(test)]
impl TaskMutexWaitTarget for TestThreadTarget {
    fn sleep(self: Arc<Self>, completion: &WaitCompletion, _key: TaskMutexWaitKey) {
        if !completion.begin_arming() || completion.finish_arming() {
            return;
        }
        while !completion.is_complete() {
            std::thread::park();
        }
    }

    fn wake(self: Arc<Self>, _key: TaskMutexWaitKey) {
        self.0.unpark();
    }
}

/// @description task mutex waiter metadata 分配失败。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TaskMutexOutOfMemory;

struct Waiter {
    key: TaskMutexWaitKey,
    completion: WaitCompletion,
    target: Option<Arc<dyn TaskMutexWaitTarget>>,
    next: spin::Mutex<Option<Arc<Waiter>>>,
}

impl Waiter {
    fn allocate() -> Result<Arc<Self>, TaskMutexOutOfMemory> {
        let completion = WaitCompletion::new();
        Arc::try_new(Self {
            key: TaskMutexWaitKey {
                owner: 0,
                ticket: 0,
            },
            completion,
            target: None,
            next: spin::Mutex::new(None),
        })
        .map_err(|_| TaskMutexOutOfMemory)
    }

    fn wait(self: &Arc<Self>) {
        self.target
            .as_ref()
            .cloned()
            .expect("task mutex waiter target disappeared before sleep")
            .sleep(&self.completion, self.key);
    }

    fn publish(self: Arc<Self>) -> Option<Wake> {
        self.completion.complete().then(|| Wake {
            target: self
                .target
                .as_ref()
                .cloned()
                .expect("task mutex waiter target disappeared before wake"),
            key: self.key,
        })
    }
}

/// @description 在不可回滚 transaction 前预分配的一份 task-mutex waiter metadata。
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
    /// @description 为当前 task 预分配可复用 waiter metadata。
    /// @errors control block 分配失败返回 OutOfMemory。
    pub(crate) fn prepare() -> Result<Self, TaskMutexOutOfMemory> {
        Ok(Self {
            waiter: Waiter::allocate()?,
        })
    }

    fn arm<T: ?Sized>(&mut self, mutex: &TaskMutex<T>) -> Arc<Waiter> {
        let waiter = Arc::get_mut(&mut self.waiter)
            .expect("task mutex wait preparation reused before acquisition");
        let ticket = mutex
            .next_ticket
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                current.checked_add(1)
            })
            .expect("task mutex waiter ticket exhausted");
        waiter.key = TaskMutexWaitKey {
            owner: core::ptr::from_ref(mutex).cast::<()>() as usize,
            ticket,
        };
        waiter.target =
            Some(current_wait_target().expect("contended task mutex outside task context"));
        waiter.completion.reset();
        waiter.next = spin::Mutex::new(None);
        self.waiter.clone()
    }

    fn disarm(&mut self, waiter: Arc<Waiter>) {
        waiter.completion.complete();
        drop(waiter);
        Arc::get_mut(&mut self.waiter)
            .expect("task mutex waiter retained after acquisition")
            .target = None;
    }
}

struct Wake {
    target: Arc<dyn TaskMutexWaitTarget>,
    key: TaskMutexWaitKey,
}

impl Wake {
    fn wake(self) {
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

/// @description 可跨调度和可睡眠 I/O 保活的 task-context mutex。
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
    /// @description 创建未锁定的 task mutex。
    pub(crate) const fn new(value: T) -> Self {
        Self {
            state: spin::Mutex::new(LockState::new()),
            next_ticket: AtomicU64::new(1),
            value: UnsafeCell::new(value),
        }
    }
}

impl<T: ?Sized> TaskMutex<T> {
    /// @description 阻塞取得 task-context guard。
    /// @return 独占 guard。
    /// @errors waiter metadata 分配失败时返回 `TaskMutexOutOfMemory`。
    pub(crate) fn lock(&self) -> Result<TaskMutexGuard<'_, T>, TaskMutexOutOfMemory> {
        if let Some(guard) = self.try_lock() {
            return Ok(guard);
        }
        let mut preparation = TaskMutexWaitPreparation::prepare()?;
        Ok(self.lock_prepared(&mut preparation))
    }

    /// @description 使用 caller 已预分配的 waiter metadata 阻塞取得 guard。
    /// @param preparation 当前 task 独占、且前一次 acquisition 已完成的 preparation。
    /// @return 独占 guard；本阶段不再分配，因此适用于不可回滚 transaction 的提交尾部。
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

    /// @description 仅在当前无 owner 时取得 guard，不排队、不分配。
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

/// @description `TaskMutex` 的 task-context 独占 guard。
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
}
