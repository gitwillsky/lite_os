use core::cmp::Ordering;

use alloc::{collections::binary_heap::BinaryHeap, sync::Arc};

use crate::task::TaskControlBlock;

/// @description 唯一生效的 cooperative vruntime runqueue。
pub(crate) struct CfsRunQueue {
    tasks: BinaryHeap<RunQueueEntry>,
}

/// @description 带 enqueue generation 的唯一 runqueue membership token。
#[derive(Debug)]
pub(crate) struct RunQueueEntry {
    pub(crate) task: Arc<TaskControlBlock>,
    pub(crate) generation: u64,
    pub(crate) vruntime: u64,
}

impl CfsRunQueue {
    /// @description 在 scheduler 发布前为所有可能 live Thread 预留 heap storage。
    /// @param capacity 由物理页数与每 Thread kernel-stack 页数推导的上界。
    /// @return 成功返回空 runqueue；heap OOM 返回错误。
    pub(crate) fn try_with_capacity(capacity: usize) -> Result<Self, ()> {
        let mut tasks = BinaryHeap::new();
        tasks.try_reserve_exact(capacity).map_err(|_| ())?;
        Ok(Self { tasks })
    }

    /// @description 插入已经声明为该 CPU Ready 的 task。
    ///
    /// @param entry runqueue 获得的 membership token 与 task owner。
    /// @return 无返回值。
    pub(crate) fn push(&mut self, entry: RunQueueEntry) {
        assert!(
            self.tasks.len() < self.tasks.capacity(),
            "preallocated runqueue capacity exhausted"
        );
        self.tasks.push(entry);
    }

    /// @description 原地清理失效 generation，保留 heap backing storage。
    /// @param keep 判定 entry 是否仍拥有 Ready membership。
    /// @return 删除的 stale entry 数量。
    pub(crate) fn retain(&mut self, keep: impl FnMut(&RunQueueEntry) -> bool) -> usize {
        let before = self.tasks.len();
        self.tasks.retain(keep);
        before - self.tasks.len()
    }

    /// @description 取出 vruntime 最小的 task。
    ///
    /// @return 队列为空时为 None，否则返回被移除的 membership owner。
    pub(crate) fn pop(&mut self) -> Option<RunQueueEntry> {
        self.tasks.pop()
    }

    /// @description 返回当前 Ready heap 的最小 vruntime，用于新 task 的公平 placement。
    ///
    /// @return 队列为空时为 `None`。
    pub(in crate::task) fn minimum_vruntime(&self) -> Option<u64> {
        self.tasks.peek().map(|entry| entry.vruntime)
    }

    /// @description 返回真实 local heap entry 数。
    ///
    /// @return 当前容器长度。
    pub(crate) fn len(&self) -> usize {
        self.tasks.len()
    }
}

impl PartialEq for RunQueueEntry {
    fn eq(&self, other: &Self) -> bool {
        self.vruntime == other.vruntime
            && self.generation == other.generation
            && self.task.tid() == other.task.tid()
    }
}

impl Eq for RunQueueEntry {}

impl PartialOrd for RunQueueEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for RunQueueEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        // 1. vruntime 小者优先；2. TID/generation 形成稳定全序，避免 Ord 与 Eq 不一致。
        let by_vruntime = other.vruntime.cmp(&self.vruntime);
        by_vruntime
            .then_with(|| other.task.tid().cmp(&self.task.tid()))
            .then_with(|| other.generation.cmp(&self.generation))
    }
}
