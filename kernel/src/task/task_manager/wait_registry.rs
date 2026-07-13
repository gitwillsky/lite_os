use alloc::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    vec::Vec,
};
use lazy_static::lazy_static;

use super::{IndexedWaitKind, PollWaitKey};
use crate::{
    ipc::{Pipe, PipeDirection},
    memory::FutexKey,
    sync::IrqMutex,
    task::TaskControlBlock,
};

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
    ) -> Option<Option<usize>> {
        match self.kind {
            IndexedWaitKind::Pipe {
                identity: candidate,
                direction: candidate_direction,
            } if candidate == identity && candidate_direction == direction => Some(None),
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
    entries: BTreeMap<u64, IndexedWaitEntry>,
    futex_index: BTreeSet<(FutexKey, u64)>,
    deadline_index: BTreeSet<(u64, u64)>,
    // bool 是 exclusive mode；缺失它会把普通 wake-all 和 EPOLLEXCLUSIVE wake-one 混为一轨。
    console_index: BTreeSet<(bool, u64)>,
    pipe_index: BTreeSet<(usize, u8, bool, u64)>,
}

impl IndexedWaitQueue {
    fn new() -> Self {
        Self {
            next_id: 0,
            entries: BTreeMap::new(),
            futex_index: BTreeSet::new(),
            deadline_index: BTreeSet::new(),
            console_index: BTreeSet::new(),
            pipe_index: BTreeSet::new(),
        }
    }

    fn allocate_id(&mut self) -> u64 {
        self.next_id = self.next_id.wrapping_add(1);
        assert_ne!(self.next_id, 0, "indexed wait ID wrapped");
        self.next_id
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

    pub(super) fn insert_deadline(&mut self, deadline: u64, task: Arc<TaskControlBlock>) -> u64 {
        let id = self.allocate_id();
        assert!(self.deadline_index.insert((deadline, id)));
        assert!(
            self.entries
                .insert(
                    id,
                    IndexedWaitEntry {
                        task,
                        kind: IndexedWaitKind::Deadline,
                        deadline: Some(deadline),
                        poll_keys: None,
                    },
                )
                .is_none()
        );
        id
    }

    /// @description 把 futex waiter 发布到 key 与可选 deadline 的唯一索引。
    ///
    /// @param key memory domain 已归一化的 futex identity。
    /// @param bitset waiter 接受的非零 wake mask。
    /// @param deadline 可选 absolute monotonic deadline。
    /// @param task 被阻塞的 Thread owner。
    /// @return 新 wait membership ID。
    pub(super) fn insert_futex(
        &mut self,
        key: FutexKey,
        bitset: u32,
        deadline: Option<u64>,
        task: Arc<TaskControlBlock>,
    ) -> u64 {
        let id = self.allocate_id();
        assert!(self.futex_index.insert((key, id)));
        if let Some(deadline) = deadline {
            assert!(self.deadline_index.insert((deadline, id)));
        }
        assert!(
            self.entries
                .insert(
                    id,
                    IndexedWaitEntry {
                        task,
                        kind: IndexedWaitKind::Futex { key, bitset },
                        deadline,
                        poll_keys: None,
                    },
                )
                .is_none()
        );
        id
    }

    pub(super) fn insert_console(&mut self, task: Arc<TaskControlBlock>) -> u64 {
        let id = self.allocate_id();
        assert!(self.console_index.insert((false, id)));
        assert!(
            self.entries
                .insert(
                    id,
                    IndexedWaitEntry {
                        task,
                        kind: IndexedWaitKind::Console,
                        deadline: None,
                        poll_keys: None,
                    },
                )
                .is_none()
        );
        id
    }

    pub(super) fn insert_signal(
        &mut self,
        mask: u64,
        deadline: Option<u64>,
        task: Arc<TaskControlBlock>,
    ) -> u64 {
        let id = self.allocate_id();
        if let Some(deadline) = deadline {
            assert!(self.deadline_index.insert((deadline, id)));
        }
        assert!(
            self.entries
                .insert(
                    id,
                    IndexedWaitEntry {
                        task,
                        kind: IndexedWaitKind::Signal { mask },
                        deadline,
                        poll_keys: None,
                    },
                )
                .is_none()
        );
        id
    }

    pub(super) fn insert_pipe(
        &mut self,
        pipe: &Arc<Pipe>,
        direction: PipeDirection,
        task: Arc<TaskControlBlock>,
    ) -> u64 {
        let id = self.allocate_id();
        let identity = Pipe::identity(pipe);
        assert!(
            self.pipe_index
                .insert((identity, direction as u8, false, id))
        );
        assert!(
            self.entries
                .insert(
                    id,
                    IndexedWaitEntry {
                        task,
                        kind: IndexedWaitKind::Pipe {
                            identity,
                            direction,
                        },
                        deadline: None,
                        poll_keys: None,
                    },
                )
                .is_none()
        );
        id
    }

    pub(super) fn insert_poll(
        &mut self,
        keys: Vec<PollWaitKey>,
        deadline: Option<u64>,
        task: Arc<TaskControlBlock>,
    ) -> u64 {
        let id = self.allocate_id();
        for key in &keys {
            match *key {
                PollWaitKey::Console { exclusive, .. } => {
                    assert!(self.console_index.insert((exclusive, id)))
                }
                PollWaitKey::Pipe {
                    identity,
                    direction,
                    exclusive,
                    ..
                } => assert!(
                    self.pipe_index
                        .insert((identity, direction as u8, exclusive, id,))
                ),
            }
        }
        if let Some(deadline) = deadline {
            assert!(self.deadline_index.insert((deadline, id)));
        }
        assert!(
            self.entries
                .insert(
                    id,
                    IndexedWaitEntry {
                        task,
                        kind: IndexedWaitKind::Poll,
                        deadline,
                        poll_keys: Some(keys),
                    },
                )
                .is_none()
        );
        id
    }

    pub(super) fn remove(&mut self, id: u64) -> Option<IndexedWaitEntry> {
        let entry = self.entries.remove(&id)?;
        if let IndexedWaitKind::Futex { key, .. } = entry.kind {
            assert!(self.futex_index.remove(&(key, id)));
        }
        if matches!(entry.kind, IndexedWaitKind::Console) {
            assert!(self.console_index.remove(&(false, id)));
        }
        if let IndexedWaitKind::Pipe {
            identity,
            direction,
        } = entry.kind
        {
            assert!(
                self.pipe_index
                    .remove(&(identity, direction as u8, false, id))
            );
        }
        if let Some(keys) = &entry.poll_keys {
            for key in keys {
                match *key {
                    PollWaitKey::Console { exclusive, .. } => {
                        assert!(self.console_index.remove(&(exclusive, id)))
                    }
                    PollWaitKey::Pipe {
                        identity,
                        direction,
                        exclusive,
                        ..
                    } => {
                        assert!(
                            self.pipe_index
                                .remove(&(identity, direction as u8, exclusive, id,))
                        )
                    }
                }
            }
        }
        if let Some(deadline) = entry.deadline {
            assert!(self.deadline_index.remove(&(deadline, id)));
        }
        Some(entry)
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
    ) -> Option<(u64, Arc<TaskControlBlock>)> {
        let (_, id) = *self
            .futex_index
            .range((key, 0)..=(key, u64::MAX))
            .find(|(_, id)| {
                matches!(
                    self.entries.get(id).map(|entry| entry.kind),
                    Some(IndexedWaitKind::Futex { bitset: waiter, .. })
                        if waiter & bitset != 0
                )
            })?;
        self.remove(id).map(|entry| (id, entry.task))
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
        let ids: Vec<_> = self
            .futex_index
            .range((source, 0)..=(source, u64::MAX))
            .take(count)
            .map(|(_, id)| *id)
            .collect();
        for id in &ids {
            assert!(self.futex_index.remove(&(source, *id)));
            let entry = self
                .entries
                .get_mut(id)
                .expect("futex index must reference a live entry");
            let IndexedWaitKind::Futex { key, .. } = &mut entry.kind else {
                panic!("futex index referenced a non-futex entry");
            };
            assert_eq!(*key, source);
            *key = target;
            assert!(self.futex_index.insert((target, *id)));
        }
        ids.len()
    }

    pub(super) fn pop_expired(
        &mut self,
        now: u64,
    ) -> Option<(u64, Arc<TaskControlBlock>, IndexedWaitKind)> {
        let (deadline, id) = *self.deadline_index.first()?;
        if deadline > now {
            return None;
        }
        self.remove(id).map(|entry| (id, entry.task, entry.kind))
    }

    pub(super) fn take_console(
        &mut self,
        exclusive: bool,
        ready: i16,
        excluded_groups: &BTreeSet<usize>,
    ) -> Option<(u64, IndexedWaitEntry, Option<usize>)> {
        let id = self
            .console_index
            .range((exclusive, 0)..=(exclusive, u64::MAX))
            .map(|(_, id)| *id)
            .find(|id| {
                self.entries
                    .get(id)
                    .and_then(|entry| entry.console_wake_group(ready))
                    .is_some_and(|group| {
                        group.is_none_or(|group| !excluded_groups.contains(&group))
                    })
            })?;
        let group = self.entries.get(&id)?.console_wake_group(ready)?;
        self.remove(id).map(|entry| (id, entry, group))
    }

    pub(super) fn take_pipe(
        &mut self,
        identity: usize,
        direction: PipeDirection,
        exclusive: bool,
        ready: i16,
        excluded_groups: &BTreeSet<usize>,
    ) -> Option<(u64, IndexedWaitEntry, Option<usize>)> {
        let id = self
            .pipe_index
            .range(
                (identity, direction as u8, exclusive, 0)
                    ..=(identity, direction as u8, exclusive, u64::MAX),
            )
            .map(|(_, _, _, id)| *id)
            .find(|id| {
                self.entries
                    .get(id)
                    .and_then(|entry| entry.pipe_wake_group(identity, direction, ready))
                    .is_some_and(|group| {
                        group.is_none_or(|group| !excluded_groups.contains(&group))
                    })
            })?;
        let group = self
            .entries
            .get(&id)?
            .pipe_wake_group(identity, direction, ready)?;
        self.remove(id).map(|entry| (id, entry, group))
    }
}

lazy_static! {
    // OWNER: wait registry owns one membership plus all source/deadline indexes；mode bit only
    // changes wake selection，缺失它会把 EPOLLEXCLUSIVE 退化为 wake-all。
    pub(super) static ref INDEXED_WAIT_QUEUE: IrqMutex<IndexedWaitQueue> =
        IrqMutex::new(IndexedWaitQueue::new());
}
