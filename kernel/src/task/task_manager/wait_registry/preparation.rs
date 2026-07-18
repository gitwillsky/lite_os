use alloc::{sync::Arc, vec::Vec};

use super::{
    IndexedWaitEntry, IndexedWaitKind, PollWaitKey, PreparedWait, WaitIndexKey, WaitTicket,
};
use crate::{
    fallible_tree::{FallibleMap, VacantEntry},
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
        index_count: usize,
        prepare_indexes: impl FnOnce(u64, &mut Vec<VacantEntry<WaitIndexKey, ()>>) -> Result<(), ()>,
    ) -> Result<PreparedWait, ()> {
        // 1. ticket 不在 registry 中；Vec 与全部 AVL node 在无 IrqMutex guard 时准备。
        let mut indexes = Vec::new();
        indexes.try_reserve_exact(index_count).map_err(|_| ())?;
        prepare_indexes(self.id, &mut indexes)?;
        debug_assert_eq!(indexes.len(), index_count);
        let entry = FallibleMap::try_prepare(
            self.id,
            IndexedWaitEntry {
                task,
                kind,
                deadline,
                poll_keys,
            },
        )
        .map_err(|_| ())?;
        Ok(PreparedWait {
            id: self.id,
            entry,
            indexes,
        })
    }

    fn prepare_index(
        indexes: &mut Vec<VacantEntry<WaitIndexKey, ()>>,
        key: WaitIndexKey,
    ) -> Result<(), ()> {
        indexes.push(FallibleMap::try_prepare(key, ()).map_err(|_| ())?);
        Ok(())
    }

    fn prepare_optional_deadline(
        indexes: &mut Vec<VacantEntry<WaitIndexKey, ()>>,
        id: u64,
        deadline: Option<u64>,
    ) -> Result<(), ()> {
        deadline.map_or(Ok(()), |deadline| {
            Self::prepare_index(indexes, WaitIndexKey::Deadline { deadline, id })
        })
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
            1,
            |id, indexes| Self::prepare_index(indexes, WaitIndexKey::Deadline { deadline, id }),
        )
    }

    pub(in crate::task::task_manager) fn prepare_futex(
        self,
        key: FutexKey,
        bitset: u32,
        deadline: Option<u64>,
        task: Arc<TaskControlBlock>,
    ) -> Result<PreparedWait, ()> {
        self.prepare(
            task,
            IndexedWaitKind::Futex { key, bitset },
            deadline,
            None,
            1 + usize::from(deadline.is_some()),
            |id, indexes| {
                Self::prepare_index(indexes, WaitIndexKey::Futex { key, id })?;
                Self::prepare_optional_deadline(indexes, id, deadline)
            },
        )
    }

    pub(in crate::task::task_manager) fn prepare_console(
        self,
        deadline: Option<u64>,
        task: Arc<TaskControlBlock>,
    ) -> Result<PreparedWait, ()> {
        self.prepare(
            task,
            IndexedWaitKind::Console,
            deadline,
            None,
            1 + usize::from(deadline.is_some()),
            |id, indexes| {
                Self::prepare_index(
                    indexes,
                    WaitIndexKey::Console {
                        exclusive: false,
                        id,
                    },
                )?;
                Self::prepare_optional_deadline(indexes, id, deadline)
            },
        )
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
            usize::from(deadline.is_some()),
            |id, indexes| Self::prepare_optional_deadline(indexes, id, deadline),
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
        let direction = condition.direction();
        self.prepare(
            task,
            IndexedWaitKind::Pipe {
                identity,
                condition,
            },
            deadline,
            None,
            1 + usize::from(deadline.is_some()),
            |id, indexes| {
                Self::prepare_index(
                    indexes,
                    WaitIndexKey::Pipe {
                        identity,
                        direction: direction as u8,
                        exclusive: false,
                        id,
                    },
                )?;
                Self::prepare_optional_deadline(indexes, id, deadline)
            },
        )
    }

    pub(in crate::task::task_manager) fn prepare_advisory_lock(
        self,
        key: AdvisoryLockKey,
        task: Arc<TaskControlBlock>,
    ) -> Result<PreparedWait, ()> {
        self.prepare(
            task,
            IndexedWaitKind::AdvisoryLock { key },
            None,
            None,
            1,
            |id, indexes| Self::prepare_index(indexes, WaitIndexKey::AdvisoryLock { key, id }),
        )
    }

    pub(in crate::task::task_manager) fn prepare_poll(
        self,
        keys: Vec<PollWaitKey>,
        deadline: Option<u64>,
        task: Arc<TaskControlBlock>,
    ) -> Result<PreparedWait, ()> {
        let index_count = keys
            .len()
            .checked_add(usize::from(deadline.is_some()))
            .ok_or(())?;
        let mut indexes = Vec::new();
        indexes.try_reserve_exact(index_count).map_err(|_| ())?;
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
                    direction: direction as u8,
                    exclusive,
                    id: self.id,
                },
            };
            Self::prepare_index(&mut indexes, index)?;
        }
        Self::prepare_optional_deadline(&mut indexes, self.id, deadline)?;
        let entry = FallibleMap::try_prepare(
            self.id,
            IndexedWaitEntry {
                task,
                kind: IndexedWaitKind::Poll,
                deadline,
                poll_keys: Some(keys),
            },
        )
        .map_err(|_| ())?;
        Ok(PreparedWait {
            id: self.id,
            entry,
            indexes,
        })
    }
}
