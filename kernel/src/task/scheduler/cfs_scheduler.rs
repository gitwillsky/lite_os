use core::cmp::Ordering;

use alloc::sync::Arc;

use crate::task::TaskControlBlock;

#[path = "preallocated_heap.rs"]
mod preallocated_heap;

use preallocated_heap::PreallocatedHeap;

/// @description 唯一生效的 cooperative vruntime runqueue。
pub(crate) struct CfsRunQueue {
    tasks: PreallocatedHeap<RunQueueEntry>,
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
        Ok(Self {
            tasks: PreallocatedHeap::try_with_capacity(capacity)?,
        })
    }

    /// @description 仅在 backing capacity 不足时原地清理失效 generation。
    /// @param additional 即将插入的 entry 数。
    /// @param keep 判定 entry 是否仍拥有 Ready membership。
    /// @return 删除的 stale entry 数；有 spare capacity 时固定为零且不调用 keep。
    #[inline(always)]
    pub(crate) fn make_room(
        &mut self,
        additional: usize,
        keep: impl FnMut(&RunQueueEntry) -> bool,
    ) -> usize {
        self.tasks.make_room(additional, keep)
    }

    /// @description 清除连续 stale heap root，使 minimum vruntime 对应 live token。
    /// @param keep 判定 root 是否仍拥有 Ready membership。
    /// @return 删除的 stale root 数。
    #[inline(always)]
    pub(crate) fn discard_stale_roots(
        &mut self,
        keep: impl FnMut(&RunQueueEntry) -> bool,
    ) -> usize {
        self.tasks.discard_invalid_roots(keep)
    }

    /// @description 插入已经完成 capacity proof 的 Ready token。
    pub(crate) fn push(&mut self, entry: RunQueueEntry) {
        self.tasks.push(entry);
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
