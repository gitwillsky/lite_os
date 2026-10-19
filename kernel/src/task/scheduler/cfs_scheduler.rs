//! 每 CPU 的就绪队列：按 vruntime 排序的最小堆，过期项惰性删除。
//!
//! # 它解决什么问题
//!
//! 完全公平调度（CFS）的想法只有一句：每个任务累计一个“虚拟运行时间” vruntime，调度器总是挑
//! vruntime 最小的任务运行——跑得少的先跑，长期看每个任务得到的 CPU 份额与它的权重成正比。
//!
//! # vruntime 怎么来
//!
//! 任务每结束一段 CPU 时间片，就把运行时间按 nice 对应的权重折算后加到 vruntime 上（nice 0 时
//! 1 µs 对应 1024 个单位，权重越大涨得越慢，见 `task/model/scheduling.rs`）。新建任务放置到某个
//! CPU 时，vruntime 不得低于该 CPU 队列当前的下限，否则新任务会凭空领先已有任务。
//!
//! # 数据结构：堆 + 惰性删除
//!
//! 就绪任务放在二叉堆里，堆顶是 vruntime 最小者。`Ord` 故意把比较反过来，因为标准库的
//! `BinaryHeap` 是最大堆；vruntime 相同时用 tid、generation 决出稳定的全序。
//!
//! 麻烦在于任务会离开队列：被阻塞、迁移到别的 CPU、被停止。从二叉堆中间删除一个元素需要 O(n) 的
//! 查找，所以这里**不删**：
//!
//! 1. 任务每次进入 Ready 都在它的 `SchedulingState` 里得到一个新的 generation，唯一的权威状态是
//!    `RunState::Ready { cpu, generation }`。
//! 2. 队列里的 `RunQueueEntry` 记着入队时的 generation。
//! 3. 旧 entry 留在堆里变成“过期”：只有 cpu 与 generation 都和权威状态一致，才算有效
//!    （`RunQueueEntry::is_current_ready`，见 `task/processor/ready_queue.rs`）。
//! 4. 过期 entry 浮到堆顶时被丢弃（`discard_stale_roots`），所以调度器看到的最小 vruntime 总来自
//!    有效 entry。
//!
//! 代价是堆里会暂存一些过期 entry，它们占用的是预先分配好的空间。
//!
//! # 为什么堆容量在启动时一次性预留
//!
//! 入队在调度器的热路径上，不能分配；若入队因 OOM 失败，就会丢掉一个本该运行的任务。
//! `PreallocatedHeap` 按“可能同时存在的 Thread 数上界”（由物理页数与每个 Thread 的 kernel stack
//! 页数推导）在启动时预留存储，运行期 `push` 只做原地操作。物理容量不足时 `make_room` 才一次性
//! 清掉过期 entry 腾位置（有空位时不会被调用）；清完仍不够就 panic，那表示容量上界推导有 bug。
//!
//! # 抢占与负载均衡
//!
//! `local_ready_preempts` 只在队首 vruntime **严格小于**当前任务时才抢占，相等不抢。
//!
//! 每个 CPU 一个队列，任务被抢占后重新入队时按各 CPU 的负载重新选核，所以大的负载差会被自动修正。负载并列时
//! 倾向留在原 CPU，这会把 N+1 个任务在 N 个 CPU 上的分布锁死；因此每隔固定次数的抢占会放宽这个倾向，让任务在
//! 并列的 CPU 之间轮转（见 `placement_policy`）。已知局限：没有跨 CPU 拉取或推送 Ready 任务，详见
//! `docs/architecture/process-scheduling.md` 的 Known limits。
//!
//! # 阅读顺序
//!
//! `task/model/scheduling.rs` 的 `transition_to_ready`（generation 从哪来）→ 本文件 →
//! `preallocated_heap.rs` → `task/processor/ready_queue.rs`（入队与选择）。

use core::cmp::Ordering;

use alloc::sync::Arc;

use crate::task::TaskControlBlock;

use super::preallocated_heap::PreallocatedHeap;

/// 唯一生效的 cooperative vruntime runqueue。
pub(crate) struct CfsRunQueue {
    tasks: PreallocatedHeap<RunQueueEntry>,
}

/// 带 enqueue generation 的唯一 runqueue membership token。
#[derive(Debug)]
pub(crate) struct RunQueueEntry {
    pub(crate) task: Arc<TaskControlBlock>,
    pub(crate) generation: u64,
    pub(crate) vruntime: u64,
}

impl CfsRunQueue {
    /// 在 scheduler 发布前为所有可能 live Thread 预留 heap storage。
    ///
    /// # Parameters
    ///
    /// - `capacity`: 由物理页数与每 Thread kernel-stack 页数推导的上界。
    ///
    /// # Returns
    ///
    /// 成功返回空 runqueue；heap OOM 返回错误。
    pub(crate) fn try_with_capacity(capacity: usize) -> Result<Self, ()> {
        Ok(Self {
            tasks: PreallocatedHeap::try_with_capacity(capacity)?,
        })
    }

    /// 仅在 backing capacity 不足时原地清理失效 generation。
    ///
    /// # Parameters
    ///
    /// - `additional`: 即将插入的 entry 数。
    /// - `keep`: 判定 entry 是否仍拥有 Ready membership。
    ///
    /// # Returns
    ///
    /// 删除的 stale entry 数；有 spare capacity 时固定为零且不调用 keep。
    #[inline(always)]
    pub(crate) fn make_room(
        &mut self,
        additional: usize,
        keep: impl FnMut(&RunQueueEntry) -> bool,
    ) -> usize {
        self.tasks.make_room(additional, keep)
    }

    /// 清除连续 stale heap root，使 minimum vruntime 对应 live token。
    ///
    /// # Parameters
    ///
    /// - `keep`: 判定 root 是否仍拥有 Ready membership。
    ///
    /// # Returns
    ///
    /// 删除的 stale root 数。
    #[inline(always)]
    pub(crate) fn discard_stale_roots(
        &mut self,
        keep: impl FnMut(&RunQueueEntry) -> bool,
    ) -> usize {
        self.tasks.discard_invalid_roots(keep)
    }

    /// 插入已经完成 capacity proof 的 Ready token。
    pub(crate) fn push(&mut self, entry: RunQueueEntry) {
        self.tasks.push(entry);
    }

    /// 取出 vruntime 最小的 task。
    ///
    /// # Returns
    ///
    /// 队列为空时为 None，否则返回被移除的 membership owner。
    pub(crate) fn pop(&mut self) -> Option<RunQueueEntry> {
        self.tasks.pop()
    }

    /// 返回当前 Ready heap 的最小 vruntime，用于新 task 的公平 placement。
    ///
    /// # Returns
    ///
    /// 队列为空时为 `None`。
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
