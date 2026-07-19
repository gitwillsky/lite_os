use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::AtomicU8;
use spin::Mutex;

use super::{
    IndexedWaitKind, PollWaitKey, PreparedIndex, PreparedWait, WaitIndexKey, WaitRegistration,
    WaitTicket, pipe_direction, registration::PREPARED,
};
use crate::{
    fallible_tree::FallibleMap,
    fs::AdvisoryLockKey,
    ipc::{Pipe, PipeWaitCondition},
    memory::FutexKey,
    task::TaskControlBlock,
};

impl WaitTicket {
    fn prepare(
        self,
        task: Arc<TaskControlBlock>,
        kind: IndexedWaitKind,
        deadline: Option<u64>,
        poll_keys: Option<Vec<PollWaitKey>>,
        mut keys: Vec<WaitIndexKey>,
    ) -> Result<PreparedWait, ()> {
        keys.push(WaitIndexKey::Task {
            tid: task.tid(),
            id: self.id,
        });
        if let Some(deadline) = deadline {
            keys.push(WaitIndexKey::Deadline {
                deadline,
                id: self.id,
            });
        }
        let registration = Arc::try_new(WaitRegistration {
            id: self.id,
            task,
            kind: Mutex::new(kind),
            poll_keys,
            keys: Mutex::new(keys),
            state: AtomicU8::new(PREPARED),
        })
        .map_err(|_| ())?;
        let key_count = registration.keys.lock().len();
        let mut indexes = Vec::new();
        indexes.try_reserve_exact(key_count).map_err(|_| ())?;
        for key in registration.keys.lock().iter().copied() {
            indexes.push(PreparedIndex {
                shard: key.shard(),
                node: FallibleMap::try_prepare(key, registration.clone()).map_err(|_| ())?,
            });
        }
        Ok(PreparedWait {
            registration,
            indexes,
        })
    }

    fn keys(capacity: usize) -> Result<Vec<WaitIndexKey>, ()> {
        let mut keys = Vec::new();
        keys.try_reserve_exact(capacity).map_err(|_| ())?;
        Ok(keys)
    }

    pub(in crate::task::task_manager) fn prepare_deadline(
        self,
        deadline: u64,
        task: Arc<TaskControlBlock>,
    ) -> Result<PreparedWait, ()> {
        self.prepare(
            task,
            IndexedWaitKind::Deadline,
            Some(deadline),
            None,
            Self::keys(2)?,
        )
    }

    pub(in crate::task::task_manager) fn prepare_futex(
        self,
        key: FutexKey,
        bitset: u32,
        deadline: Option<u64>,
        task: Arc<TaskControlBlock>,
    ) -> Result<PreparedWait, ()> {
        let mut keys = Self::keys(2 + usize::from(deadline.is_some()))?;
        keys.push(WaitIndexKey::Futex { key, id: self.id });
        self.prepare(
            task,
            IndexedWaitKind::Futex { key, bitset },
            deadline,
            None,
            keys,
        )
    }

    pub(in crate::task::task_manager) fn prepare_console(
        self,
        deadline: Option<u64>,
        task: Arc<TaskControlBlock>,
    ) -> Result<PreparedWait, ()> {
        let mut keys = Self::keys(2 + usize::from(deadline.is_some()))?;
        keys.push(WaitIndexKey::Console {
            exclusive: false,
            id: self.id,
        });
        self.prepare(task, IndexedWaitKind::Console, deadline, None, keys)
    }

    pub(in crate::task::task_manager) fn prepare_signal(
        self,
        mask: u64,
        deadline: Option<u64>,
        task: Arc<TaskControlBlock>,
    ) -> Result<PreparedWait, ()> {
        self.prepare(
            task,
            IndexedWaitKind::Signal { mask },
            deadline,
            None,
            Self::keys(1 + usize::from(deadline.is_some()))?,
        )
    }

    pub(in crate::task::task_manager) fn prepare_pipe(
        self,
        pipe: &Arc<Pipe>,
        condition: PipeWaitCondition,
        deadline: Option<u64>,
        task: Arc<TaskControlBlock>,
    ) -> Result<PreparedWait, ()> {
        let identity = Pipe::identity(pipe);
        let mut keys = Self::keys(2 + usize::from(deadline.is_some()))?;
        keys.push(WaitIndexKey::Pipe {
            identity,
            direction: pipe_direction(condition.direction()),
            exclusive: true,
            id: self.id,
        });
        self.prepare(
            task,
            IndexedWaitKind::Pipe {
                identity,
                condition,
            },
            deadline,
            None,
            keys,
        )
    }

    pub(in crate::task::task_manager) fn prepare_advisory_lock(
        self,
        key: AdvisoryLockKey,
        task: Arc<TaskControlBlock>,
    ) -> Result<PreparedWait, ()> {
        let mut keys = Self::keys(2)?;
        keys.push(WaitIndexKey::AdvisoryLock { key, id: self.id });
        self.prepare(task, IndexedWaitKind::AdvisoryLock, None, None, keys)
    }

    pub(in crate::task::task_manager) fn prepare_poll(
        self,
        keys: Vec<PollWaitKey>,
        deadline: Option<u64>,
        task: Arc<TaskControlBlock>,
    ) -> Result<PreparedWait, ()> {
        let capacity = keys
            .len()
            .checked_add(1 + usize::from(deadline.is_some()))
            .ok_or(())?;
        let mut indexes = Self::keys(capacity)?;
        for key in &keys {
            let index = match *key {
                PollWaitKey::Console { exclusive, .. } => WaitIndexKey::Console {
                    exclusive,
                    id: self.id,
                },
                PollWaitKey::Pipe {
                    identity,
                    direction,
                    exclusive,
                    ..
                } => WaitIndexKey::Pipe {
                    identity,
                    direction: pipe_direction(direction),
                    exclusive,
                    id: self.id,
                },
            };
            // 多个 pollfd 可以指向同一 OFD/source；poll result 仍保留每个 key，
            // 但 registry membership 必须只有一个 exact source node，否则 detach
            // 会重复删除同一 key。
            if !indexes.contains(&index) {
                indexes.push(index);
            }
        }
        self.prepare(task, IndexedWaitKind::Poll, deadline, Some(keys), indexes)
    }
}
