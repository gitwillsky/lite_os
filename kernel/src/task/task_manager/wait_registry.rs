use alloc::{sync::Arc, vec::Vec};
use lazy_static::lazy_static;

use super::{IndexedWaitKind, PollWaitKey};
use crate::{
    fallible_tree::{FallibleMap, VacantEntry},
    fs::AdvisoryLockKey,
    ipc::{Pipe, PipeDirection, PipePollState, PipeWaitCondition},
    memory::FutexKey,
    sync::IrqMutex,
    task::TaskControlBlock,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum WaitIndexKey {
    AdvisoryLock {
        key: AdvisoryLockKey,
        id: u64,
    },
    Console {
        exclusive: bool,
        id: u64,
    },
    Deadline {
        deadline: u64,
        id: u64,
    },
    Futex {
        key: FutexKey,
        id: u64,
    },
    Pipe {
        identity: usize,
        direction: u8,
        exclusive: bool,
        id: u64,
    },
}

/// 已完成全部节点分配、尚未发布 scheduling membership 的 wait transaction。
pub(super) struct PreparedWait {
    id: u64,
    entry: VacantEntry<u64, IndexedWaitEntry>,
    indexes: Vec<VacantEntry<WaitIndexKey, ()>>,
}

/// @description 一个 Task 的唯一 indexed wait membership 与反向 index metadata。
pub(super) struct IndexedWaitEntry {
    pub(super) task: Arc<TaskControlBlock>,
    pub(super) kind: IndexedWaitKind,
    deadline: Option<u64>,
    poll_keys: Option<Vec<PollWaitKey>>,
}

impl IndexedWaitEntry {
    fn console_wake_group(&self, ready: i16) -> Option<Option<usize>> {
        match self.kind {
            IndexedWaitKind::Console => Some(None),
            IndexedWaitKind::Poll => self
                .poll_keys
                .as_ref()
                .and_then(|keys| keys.iter().find(|key| key.matches_console(ready)))
                .map(|key| key.wake_group()),
            _ => None,
        }
    }

    fn pipe_wake_group(
        &self,
        identity: usize,
        direction: PipeDirection,
        ready: i16,
        state: PipePollState,
    ) -> Option<Option<usize>> {
        match self.kind {
            IndexedWaitKind::Pipe {
                identity: candidate,
                condition,
            } if candidate == identity
                && condition.direction() == direction
                && state.satisfies(condition) =>
            {
                Some(None)
            }
            IndexedWaitKind::Poll => self
                .poll_keys
                .as_ref()
                .and_then(|keys| {
                    keys.iter()
                        .find(|key| key.matches_pipe(identity, direction, ready))
                })
                .map(|key| key.wake_group()),
            _ => None,
        }
    }
}

/// @description deadline/futex/console/Pipe/Poll registration 的唯一 index owner。
pub(super) struct IndexedWaitQueue {
    next_id: u64,
    entries: FallibleMap<u64, IndexedWaitEntry>,
    // source/deadline membership 共用一个 ordered index；variant 是领域 discriminator，
    // 缺失它会恢复五棵独立分配且无法原子 publication 的 tree。
    index: FallibleMap<WaitIndexKey, ()>,
}

impl IndexedWaitQueue {
    fn new() -> Self {
        Self {
            next_id: 0,
            entries: FallibleMap::new(),
            index: FallibleMap::new(),
        }
    }

    fn allocate_id(&mut self) -> u64 {
        self.next_id = self.next_id.wrapping_add(1);
        assert_ne!(self.next_id, 0, "indexed wait ID wrapped");
        self.next_id
    }

    fn prepare_wait(
        &mut self,
        task: Arc<TaskControlBlock>,
        kind: IndexedWaitKind,
        deadline: Option<u64>,
        poll_keys: Option<Vec<PollWaitKey>>,
        index_count: usize,
        prepare_indexes: impl FnOnce(u64, &mut Vec<VacantEntry<WaitIndexKey, ()>>) -> Result<(), ()>,
    ) -> Result<PreparedWait, ()> {
        // 1. staging Vec 与每个 AVL node 都在 wait/scheduler publication 前分配。
        let mut indexes = Vec::new();
        indexes.try_reserve_exact(index_count).map_err(|_| ())?;
        let id = self.allocate_id();
        prepare_indexes(id, &mut indexes)?;
        debug_assert_eq!(indexes.len(), index_count);
        let entry = FallibleMap::try_prepare(
            id,
            IndexedWaitEntry {
                task,
                kind,
                deadline,
                poll_keys,
            },
        )
        .map_err(|_| ())?;
        Ok(PreparedWait { id, entry, indexes })
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

    /// 在 scheduling lock 内零分配发布已准备的完整 wait membership。
    pub(super) fn commit(&mut self, prepared: PreparedWait) -> u64 {
        // 2. owner lock 尚未释放，entry/index 的提交顺序对外不可见且均不会失败。
        self.entries.commit_vacant(prepared.entry);
        for index in prepared.indexes {
            self.index.commit_vacant(index);
        }
        prepared.id
    }

    /// @description 在 owner lock 内读取 signal membership 的等待 mask。
    ///
    /// @param id SchedulingState 记录的 wait ID。
    /// @return entry 仍存活时返回 mask；已被其他完成路径消费时返回 None。
    pub(super) fn signal_mask(&self, id: u64) -> Option<u64> {
        match self.entries.get(&id)?.kind {
            IndexedWaitKind::Signal { mask } => Some(mask),
            _ => panic!("signal wait membership has divergent registry kind"),
        }
    }

    pub(super) fn prepare_deadline(
        &mut self,
        deadline: u64,
        task: Arc<TaskControlBlock>,
    ) -> Result<PreparedWait, ()> {
        self.prepare_wait(
            task,
            IndexedWaitKind::Deadline,
            Some(deadline),
            None,
            1,
            |id, indexes| Self::prepare_index(indexes, WaitIndexKey::Deadline { deadline, id }),
        )
    }

    /// @description 把 futex waiter 发布到 key 与可选 deadline 的唯一索引。
    ///
    /// @param key memory domain 已归一化的 futex identity。
    /// @param bitset waiter 接受的非零 wake mask。
    /// @param deadline 可选 absolute monotonic deadline。
    /// @param task 被阻塞的 Thread owner。
    /// @return 新 wait membership ID。
    pub(super) fn prepare_futex(
        &mut self,
        key: FutexKey,
        bitset: u32,
        deadline: Option<u64>,
        task: Arc<TaskControlBlock>,
    ) -> Result<PreparedWait, ()> {
        self.prepare_wait(
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

    /// @description 发布 terminal read 的唯一 console membership 与可选 termios deadline。
    ///
    /// @param deadline VTIME 导出的 absolute monotonic deadline；无超时时为 None。
    /// @param task 被阻塞的 Thread owner。
    /// @return 新 wait membership ID。
    pub(super) fn prepare_console(
        &mut self,
        deadline: Option<u64>,
        task: Arc<TaskControlBlock>,
    ) -> Result<PreparedWait, ()> {
        self.prepare_wait(
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

    pub(super) fn prepare_signal(
        &mut self,
        mask: u64,
        deadline: Option<u64>,
        task: Arc<TaskControlBlock>,
    ) -> Result<PreparedWait, ()> {
        self.prepare_wait(
            task,
            IndexedWaitKind::Signal { mask },
            deadline,
            None,
            usize::from(deadline.is_some()),
            |id, indexes| Self::prepare_optional_deadline(indexes, id, deadline),
        )
    }

    pub(super) fn prepare_pipe(
        &mut self,
        pipe: &Arc<Pipe>,
        condition: PipeWaitCondition,
        deadline: Option<u64>,
        task: Arc<TaskControlBlock>,
    ) -> Result<PreparedWait, ()> {
        let identity = Pipe::identity(pipe);
        let direction = condition.direction();
        self.prepare_wait(
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

    pub(super) fn prepare_advisory_lock(
        &mut self,
        key: AdvisoryLockKey,
        task: Arc<TaskControlBlock>,
    ) -> Result<PreparedWait, ()> {
        self.prepare_wait(
            task,
            IndexedWaitKind::AdvisoryLock { key },
            None,
            None,
            1,
            |id, indexes| Self::prepare_index(indexes, WaitIndexKey::AdvisoryLock { key, id }),
        )
    }

    pub(super) fn prepare_poll(
        &mut self,
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
        let id = self.allocate_id();
        for key in &keys {
            let index = match *key {
                PollWaitKey::Console { exclusive, .. } => WaitIndexKey::Console { exclusive, id },
                PollWaitKey::Pipe {
                    identity,
                    direction,
                    exclusive,
                    ..
                } => WaitIndexKey::Pipe {
                    identity,
                    direction: direction as u8,
                    exclusive,
                    id,
                },
            };
            Self::prepare_index(&mut indexes, index)?;
        }
        if let Some(deadline) = deadline {
            Self::prepare_index(&mut indexes, WaitIndexKey::Deadline { deadline, id })?;
        }
        let entry = FallibleMap::try_prepare(
            id,
            IndexedWaitEntry {
                task,
                kind: IndexedWaitKind::Poll,
                deadline,
                poll_keys: Some(keys),
            },
        )
        .map_err(|_| ())?;
        Ok(PreparedWait { id, entry, indexes })
    }

    pub(super) fn remove(&mut self, id: u64) -> Option<IndexedWaitEntry> {
        self.take_detached(id).map(VacantEntry::into_value)
    }

    fn take_detached(&mut self, id: u64) -> Option<VacantEntry<u64, IndexedWaitEntry>> {
        let entry = self.entries.take_entry(&id)?;
        if let IndexedWaitKind::Futex { key, .. } = entry.value().kind {
            self.remove_index(WaitIndexKey::Futex { key, id });
        }
        if matches!(entry.value().kind, IndexedWaitKind::Console) {
            self.remove_index(WaitIndexKey::Console {
                exclusive: false,
                id,
            });
        }
        if let IndexedWaitKind::Pipe {
            identity,
            condition,
        } = entry.value().kind
        {
            let direction = condition.direction();
            self.remove_index(WaitIndexKey::Pipe {
                identity,
                direction: direction as u8,
                exclusive: false,
                id,
            });
        }
        if let IndexedWaitKind::AdvisoryLock { key } = entry.value().kind {
            self.remove_index(WaitIndexKey::AdvisoryLock { key, id });
        }
        if let Some(keys) = &entry.value().poll_keys {
            for key in keys {
                match *key {
                    PollWaitKey::Console { exclusive, .. } => {
                        self.remove_index(WaitIndexKey::Console { exclusive, id })
                    }
                    PollWaitKey::Pipe {
                        identity,
                        direction,
                        exclusive,
                        ..
                    } => self.remove_index(WaitIndexKey::Pipe {
                        identity,
                        direction: direction as u8,
                        exclusive,
                        id,
                    }),
                }
            }
        }
        if let Some(deadline) = entry.value().deadline {
            self.remove_index(WaitIndexKey::Deadline { deadline, id });
        }
        Some(entry)
    }

    fn remove_index(&mut self, key: WaitIndexKey) {
        assert!(self.index.remove(&key).is_some(), "wait index diverged");
    }

    /// @description 取出指定 key 上最早且 bitset 相交的 waiter。
    ///
    /// @param key memory domain 已归一化的 futex identity。
    /// @param bitset wake operation 的非零匹配 mask。
    /// @return 命中时返回 wait ID 与 task，并从所有索引移除；否则返回 None。
    pub(super) fn take_futex(
        &mut self,
        key: FutexKey,
        bitset: u32,
    ) -> Option<VacantEntry<u64, IndexedWaitEntry>> {
        let start = WaitIndexKey::Futex { key, id: 0 };
        let mut selected = None;
        for (index, _) in self.index.iter_from(&start) {
            let WaitIndexKey::Futex { key: candidate, id } = *index else {
                break;
            };
            if candidate != key {
                break;
            }
            if matches!(
                self.entries.get(&id).map(|entry| entry.kind),
                Some(IndexedWaitKind::Futex { bitset: waiter, .. }) if waiter & bitset != 0
            ) {
                selected = Some(id);
                break;
            }
        }
        let id = selected?;
        self.take_detached(id)
    }

    pub(super) fn take_advisory_lock(
        &mut self,
        key: AdvisoryLockKey,
    ) -> Option<VacantEntry<u64, IndexedWaitEntry>> {
        let start = WaitIndexKey::AdvisoryLock { key, id: 0 };
        let (index, _) = self.index.iter_from(&start).next()?;
        let WaitIndexKey::AdvisoryLock { key: candidate, id } = *index else {
            return None;
        };
        if candidate != key {
            return None;
        }
        self.take_detached(id)
    }

    /// @description 在唯一 registry owner 内把 source key 的 waiter 改挂到 target key。
    ///
    /// @param source 原 futex key。
    /// @param target 新 futex key。
    /// @param count 最大迁移数。
    /// @return 实际迁移数；wait ID、deadline、task membership 与 bitset 保持不变。
    pub(super) fn requeue_futex(
        &mut self,
        source: FutexKey,
        target: FutexKey,
        count: usize,
    ) -> usize {
        if count == 0 || source == target {
            return 0;
        }
        let mut moved = 0;
        while moved < count {
            let start = WaitIndexKey::Futex { key: source, id: 0 };
            let Some((&index, _)) = self.index.iter_from(&start).next() else {
                break;
            };
            let WaitIndexKey::Futex { key, id } = index else {
                break;
            };
            if key != source {
                break;
            }
            let mut index = self
                .index
                .take_entry(&index)
                .expect("selected futex index disappeared");
            let entry = self
                .entries
                .get_mut(&id)
                .expect("futex index must reference a live entry");
            let IndexedWaitKind::Futex { key, .. } = &mut entry.kind else {
                panic!("futex index referenced a non-futex entry");
            };
            assert_eq!(*key, source);
            *key = target;
            index.set_key(WaitIndexKey::Futex { key: target, id });
            self.index.commit_vacant(index);
            moved += 1;
        }
        moved
    }

    /// @description 从唯一 deadline index 摘取一个已经到期的 registration。
    ///
    /// @param now 本批次固定的 absolute monotonic 纳秒时刻。
    /// @return 最早 deadline 已到时返回完整 waiter ownership，否则返回 `None`。
    pub(super) fn pop_expired(
        &mut self,
        now: u64,
    ) -> Option<(u64, Arc<TaskControlBlock>, IndexedWaitKind)> {
        let start = WaitIndexKey::Deadline { deadline: 0, id: 0 };
        let (index, _) = self.index.iter_from(&start).next()?;
        let WaitIndexKey::Deadline { deadline, id } = *index else {
            return None;
        };
        if deadline > now {
            return None;
        }
        self.remove(id).map(|entry| (id, entry.task, entry.kind))
    }

    /// @description 查询固定时刻是否仍有尚未摘取的到期 registration。
    ///
    /// @param now 与本批 `pop_expired` 共用的 absolute monotonic 纳秒时刻。
    /// @return 最早 deadline 不晚于 `now` 时返回 true。
    pub(super) fn has_expired_deadline(&self, now: u64) -> bool {
        let start = WaitIndexKey::Deadline { deadline: 0, id: 0 };
        self.index.iter_from(&start).next().is_some_and(
            |(key, _)| matches!(key, WaitIndexKey::Deadline { deadline, .. } if *deadline <= now),
        )
    }

    pub(super) fn take_console(
        &mut self,
        exclusive: bool,
        ready: i16,
        excluded_groups: &FallibleMap<usize, ()>,
    ) -> Option<(VacantEntry<u64, IndexedWaitEntry>, Option<usize>)> {
        let start = WaitIndexKey::Console { exclusive, id: 0 };
        let id = self
            .index
            .iter_from(&start)
            .take_while(|(index, _)| {
                matches!(
                    index,
                    WaitIndexKey::Console {
                        exclusive: candidate,
                        ..
                    } if *candidate == exclusive
                )
            })
            .find_map(|(index, _)| {
                let WaitIndexKey::Console { exclusive: _, id } = *index else {
                    return None;
                };
                self.entries
                    .get(&id)
                    .and_then(|entry| entry.console_wake_group(ready))
                    .is_some_and(|group| {
                        group.is_none_or(|group| !excluded_groups.contains_key(&group))
                    })
                    .then_some(id)
            })?;
        let group = self.entries.get(&id)?.console_wake_group(ready)?;
        self.take_detached(id).map(|entry| (entry, group))
    }

    pub(super) fn take_pipe(
        &mut self,
        identity: usize,
        direction: PipeDirection,
        exclusive: bool,
        ready: i16,
        state: PipePollState,
        excluded_groups: &FallibleMap<usize, ()>,
    ) -> Option<(VacantEntry<u64, IndexedWaitEntry>, Option<usize>)> {
        let start = WaitIndexKey::Pipe {
            identity,
            direction: direction as u8,
            exclusive,
            id: 0,
        };
        let id = self
            .index
            .iter_from(&start)
            .take_while(|(index, _)| {
                matches!(
                    index,
                    WaitIndexKey::Pipe {
                        identity: candidate_identity,
                        direction: candidate_direction,
                        exclusive: candidate_exclusive,
                        ..
                    } if (*candidate_identity, *candidate_direction, *candidate_exclusive)
                        == (identity, direction as u8, exclusive)
                )
            })
            .find_map(|(index, _)| {
                let WaitIndexKey::Pipe {
                    identity: _,
                    direction: _,
                    exclusive: _,
                    id,
                } = *index
                else {
                    return None;
                };
                self.entries
                    .get(&id)
                    .and_then(|entry| entry.pipe_wake_group(identity, direction, ready, state))
                    .is_some_and(|group| {
                        group.is_none_or(|group| !excluded_groups.contains_key(&group))
                    })
                    .then_some(id)
            })?;
        let group = self
            .entries
            .get(&id)?
            .pipe_wake_group(identity, direction, ready, state)?;
        self.take_detached(id).map(|entry| (entry, group))
    }
}

lazy_static! {
    // OWNER: wait registry owns one membership plus all source/deadline indexes；mode bit only
    // changes wake selection，缺失它会把 EPOLLEXCLUSIVE 退化为 wake-all。
    pub(super) static ref INDEXED_WAIT_QUEUE: IrqMutex<IndexedWaitQueue> =
        IrqMutex::new(IndexedWaitQueue::new());
}
