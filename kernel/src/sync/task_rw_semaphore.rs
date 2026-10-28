//! 可睡眠的 task 级读写信号量（Linux `rw_semaphore`）：读者共享、写者独占，FIFO 排队，释放时直接交接。
//!
//! # 它解决什么问题
//!
//! 目录锁（Linux `i_rwsem`）要求同一目录的多个 lookup 能同时停在文件系统里（例如都在等 FUSE 守护进程），
//! 而 create/unlink/rename 独占该目录。`TaskMutex` 只能独占，会把这些 lookup 串行化。持有者可以在持锁
//! 期间睡眠；本身不保护数据，被保护的状态由调用者拥有（与 `rw_semaphore` 相同）。
//!
//! # 交接规则
//!
//! 1. 空闲时队列必为空：最后一个释放者总是先把所有权交给队首，再离开。
//! 2. 写者释放、或最后一个读者释放时：队首是写者就只交给它；队首是读者就把队首连续的读者全部交接。
//! 3. 新来的读者只在没有写者且队列为空时直接进入；队列非空就排队，排队的写者不会被后来的读者饿死。
//!
//! 交接由释放者在 state 自旋锁内改写持有状态，被唤醒者不再竞争；唤醒在自旋锁之外。waiter 与预分配
//! 复用 `TaskMutex` 的同一实现（见 `task_mutex.rs`）。
use alloc::sync::Arc;
use core::{marker::PhantomData, sync::atomic::AtomicU64};

use super::{
    TaskMutexWaitPreparation,
    task_mutex::{TaskMutexOutOfMemory, Waiter},
    task_wait::TaskWaitMode,
};

struct State {
    readers: usize,
    writer: bool,
    head: Option<Arc<Waiter>>,
    tail: Option<Arc<Waiter>>,
}

impl State {
    /// 不排队就能进入：没有写者、没有排队者，写者还要求没有读者。
    fn admits(&self, exclusive: bool) -> bool {
        !self.writer && self.head.is_none() && (!exclusive || self.readers == 0)
    }

    fn grant(&mut self, exclusive: bool) {
        if exclusive {
            self.writer = true;
        } else {
            self.readers += 1;
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

    /// 锁刚变为无人持有：按交接规则把所有权交给队首，返回被选中者组成的链（经各自 `next` 串起）。
    fn hand_off(&mut self) -> Option<Arc<Waiter>> {
        let first = self.pop()?;
        if first.exclusive {
            self.writer = true;
            return Some(first);
        }
        self.readers = 1;
        let mut chosen = first;
        while self.head.as_ref().is_some_and(|head| !head.exclusive) {
            let next = self.pop().expect("queued reader");
            self.readers += 1;
            assert!(next.next.lock().replace(chosen).is_none());
            chosen = next;
        }
        Some(chosen)
    }
}

/// 可跨调度与可睡眠 I/O 持有的 task-context 读写信号量；竞争只能发生在 task 上下文。
pub(crate) struct TaskRwSemaphore {
    // OWNER: 共享计数、写者与 FIFO 队列在同一短锁内转移；拆开会让新读者越过排队写者。
    state: spin::Mutex<State>,
    // 每次阻塞都签发新的 (owner, ticket)；否则迟到 wake 可命中同一任务的下一次等待。
    next_ticket: AtomicU64,
}

impl TaskRwSemaphore {
    /// 创建空闲的信号量。
    pub(crate) const fn new() -> Self {
        Self {
            state: spin::Mutex::new(State {
                readers: 0,
                writer: false,
                head: None,
                tail: None,
            }),
            next_ticket: AtomicU64::new(1),
        }
    }

    /// 阻塞取得共享 guard（Linux `down_read`）。
    ///
    /// # Returns
    /// 不可跨线程转移的共享 guard；Drop 归还一次读者计数。
    ///
    /// # Errors
    ///
    /// 需要排队而 waiter 分配失败时返回 `TaskMutexOutOfMemory`。
    pub(crate) fn read(&self) -> Result<TaskReadGuard<'_>, TaskMutexOutOfMemory> {
        self.acquire(false)?;
        Ok(TaskReadGuard {
            semaphore: self,
            _not_send: PhantomData,
        })
    }

    /// 阻塞取得独占 guard（Linux `down_write`）。
    ///
    /// # Returns
    /// 不可跨线程转移的独占 guard；Drop 将所有权直接交接给等待者或归还空闲状态。
    ///
    /// # Errors
    ///
    /// 需要排队而 waiter 分配失败时返回 `TaskMutexOutOfMemory`。
    pub(crate) fn write(&self) -> Result<TaskWriteGuard<'_>, TaskMutexOutOfMemory> {
        self.acquire(true)?;
        Ok(TaskWriteGuard {
            semaphore: self,
            _not_send: PhantomData,
        })
    }

    fn acquire(&self, exclusive: bool) -> Result<(), TaskMutexOutOfMemory> {
        {
            let mut state = self.state.lock();
            if state.admits(exclusive) {
                state.grant(exclusive);
                return Ok(());
            }
        }
        let mut preparation = TaskMutexWaitPreparation::prepare()?;
        let waiter = preparation.arm_for(
            core::ptr::from_ref(self) as usize,
            &self.next_ticket,
            TaskWaitMode::Uninterruptible,
            exclusive,
        );
        {
            let mut state = self.state.lock();
            if state.admits(exclusive) {
                state.grant(exclusive);
                drop(state);
                preparation.disarm(waiter);
                return Ok(());
            }
            state.push(waiter.clone());
        }
        // 醒来时释放者已在 state 锁内把所有权记到本 waiter 名下。
        waiter.wait();
        preparation.disarm(waiter);
        Ok(())
    }

    fn release(&self, exclusive: bool) {
        let mut chosen = {
            let mut state = self.state.lock();
            if exclusive {
                assert!(
                    state.writer,
                    "task rw semaphore write-released without writer"
                );
                state.writer = false;
            } else {
                assert!(
                    state.readers != 0,
                    "task rw semaphore read-released without reader"
                );
                state.readers -= 1;
            }
            if state.readers != 0 {
                return;
            }
            state.hand_off()
        };
        while let Some(waiter) = chosen {
            chosen = waiter.next.lock().take();
            if let Some(wake) = waiter.publish() {
                wake.wake();
            }
        }
    }
}

/// [`TaskRwSemaphore`] 的共享 guard。
#[must_use = "dropping the guard releases the shared hold"]
pub(crate) struct TaskReadGuard<'a> {
    semaphore: &'a TaskRwSemaphore,
    _not_send: PhantomData<*mut ()>,
}

impl Drop for TaskReadGuard<'_> {
    fn drop(&mut self) {
        self.semaphore.release(false);
    }
}

/// [`TaskRwSemaphore`] 的独占 guard。
#[must_use = "dropping the guard releases the exclusive hold"]
pub(crate) struct TaskWriteGuard<'a> {
    semaphore: &'a TaskRwSemaphore,
    _not_send: PhantomData<*mut ()>,
}

impl Drop for TaskWriteGuard<'_> {
    fn drop(&mut self) {
        self.semaphore.release(true);
    }
}

#[cfg(test)]
mod tests {
    use super::TaskRwSemaphore;
    use alloc::sync::Arc;
    use std::{sync::mpsc, thread, time::Duration};

    fn queued(semaphore: &TaskRwSemaphore) -> usize {
        let state = semaphore.state.lock();
        let mut count = 0;
        let mut cursor = state.head.clone();
        while let Some(waiter) = cursor {
            count += 1;
            cursor = waiter.next.lock().clone();
        }
        count
    }

    fn wait_until_queued(semaphore: &TaskRwSemaphore, expected: usize) {
        for _ in 0..1_000 {
            if queued(semaphore) == expected {
                return;
            }
            thread::sleep(Duration::from_millis(1));
        }
        panic!("task rw semaphore did not queue {expected} waiters");
    }

    /// 排队的写者挡住后来的读者（防饥饿）；写者释放时把它身后连续的读者成批交接，二者同时持有。
    #[test]
    fn queued_writer_blocks_new_readers_and_releases_reader_batch() {
        let semaphore = Arc::new(TaskRwSemaphore::new());
        let first = semaphore.read().expect("first reader");
        let _second = semaphore.read().expect("readers share");
        let (events_tx, events_rx) = mpsc::channel();

        let writer_semaphore = semaphore.clone();
        let writer_events = events_tx.clone();
        let (release_writer_tx, release_writer_rx) = mpsc::channel::<()>();
        let writer = thread::spawn(move || {
            let _guard = writer_semaphore.write().expect("writer");
            writer_events.send("writer").expect("report writer");
            release_writer_rx.recv().expect("release writer");
        });
        wait_until_queued(&semaphore, 1);

        let (release_readers_tx, release_readers_rx) = mpsc::channel::<()>();
        let release_readers_rx = Arc::new(std::sync::Mutex::new(release_readers_rx));
        let readers: Vec<_> = (0..2)
            .map(|_| {
                let semaphore = semaphore.clone();
                let events = events_tx.clone();
                let release = release_readers_rx.clone();
                thread::spawn(move || {
                    let _guard = semaphore.read().expect("queued reader");
                    events.send("reader").expect("report reader");
                    release.lock().unwrap().recv().expect("release reader");
                })
            })
            .collect();
        wait_until_queued(&semaphore, 3);

        drop(first);
        thread::sleep(Duration::from_millis(20));
        assert!(
            events_rx.try_recv().is_err(),
            "writer entered beside a reader"
        );
        drop(_second);
        assert_eq!(events_rx.recv().unwrap(), "writer");
        release_writer_tx.send(()).unwrap();
        assert_eq!(events_rx.recv().unwrap(), "reader");
        assert_eq!(
            events_rx.recv().unwrap(),
            "reader",
            "reader batch was split"
        );
        release_readers_tx.send(()).unwrap();
        release_readers_tx.send(()).unwrap();
        writer.join().unwrap();
        for reader in readers {
            reader.join().unwrap();
        }
        drop(semaphore.write().expect("semaphore returned to idle"));
    }
}
