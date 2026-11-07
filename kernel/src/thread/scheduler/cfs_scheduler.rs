//! 每 CPU 的就绪队列：按 vruntime 排序的最小堆，过期项惰性删除。
//!
//! # 它解决什么问题
//!
//! 完全公平调度（CFS）的想法只有一句：每个任务累计一个“虚拟运行时间” vruntime，调度器总是挑
//! vruntime 最小的任务运行——跑得少的先跑，长期看每个任务得到的 CPU 份额与它的权重成正比。
//!
//! # vruntime 怎么来
//!
//! 任务每结束一段 CPU 时间片，就把运行时间按权重等级对应的权重折算后加到 vruntime 上（默认权重时
//! 1 µs 对应 1024 个单位，权重越大涨得越慢，见 `thread/scheduling.rs`）。新建任务放置到某个
//! CPU 时，vruntime 不得低于该 CPU 已发布的单调下限；真实睡眠唤醒最多保留一个时间片的归一化信用。
//! 不截断旧信用会让长睡眠的服务重新唤醒后压过已经运行的线程，信用调整不计入 CPU 使用时间。
//!
//! # 数据结构：堆 + 惰性删除
//!
//! 就绪任务放在二叉堆里，堆顶是 vruntime 最小者。`Ord` 故意把比较反过来，因为标准库的
//! `BinaryHeap` 是最大堆；vruntime 相同时用线程 id、generation 决出稳定的全序。
//!
//! 麻烦在于任务会离开队列：被阻塞、迁移到别的 CPU。从二叉堆中间删除一个元素需要 O(n) 的
//! 查找，所以这里**不删**：
//!
//! 1. 任务每次进入 Ready 都在它的 `SchedulingState` 里得到一个新的 generation，唯一的权威状态是
//!    `RunState::Ready { cpu, generation }`。
//! 2. 队列里的 `RunQueueEntry` 记着入队时的 generation。
//! 3. 旧 entry 留在堆里变成“过期”：只有 cpu 与 generation 都和权威状态一致，才算有效
//!    （`RunQueueEntry::is_current_ready`，见 `thread/processor/ready_queue.rs`）。
//! 4. 过期 entry 浮到堆顶时被丢弃（`discard_stale_roots`），所以调度器看到的最小 vruntime 总来自
//!    有效 entry。
//!
//! 代价是堆里会暂存一些过期 entry，它们占用的是预先分配好的空间。
//!
//! # 为什么在线程创建时准备堆容量
//!
//! 入队在调度器的热路径上，不能分配。若入队因 OOM 失败，就会丢掉一个本该运行的任务。
//! 线程创建先为新的存活线程数高水位准备备用队列。关中断入队容量不足时接管这块存储，
//! 必要时再清理过期 entry。清完仍不够意味着线程接纳和实际队列容量的契约已损坏。
//!
//! # 抢占与负载均衡
//!
//! `local_ready_preempts` 只在队首 vruntime **严格小于**当前任务时才抢占，相等不抢。
//!
//! 每个 CPU 一个队列，任务被抢占后重新入队时按各 CPU 的负载重新选核，所以大的负载差会被自动修正。负载并列时
//! 倾向留在原 CPU，这会把 N+1 个任务在 N 个 CPU 上的分布锁死；因此每隔固定次数的抢占会放宽这个倾向，让任务在
//! 并列的 CPU 之间轮转（见 `placement_policy`）。刚迁入的任务累积足够的本地运行时间后才能再次参与轮转。
//! 当前跨核平衡发生在切出时。
//!
//! # 阅读顺序
//!
//! `thread/scheduling.rs` 的 `transition_to_ready`（generation 从哪来）→ 本文件 →
//! `preallocated_heap.rs` → `thread/processor/ready_queue.rs`（入队与选择）。

use core::cmp::Ordering;

use alloc::sync::Arc;

use crate::thread::Thread;

use super::preallocated_heap::PreallocatedHeap;

/// 每 CPU 的 vruntime runqueue。
pub(crate) struct CfsRunQueue {
    tasks: PreallocatedHeap<RunQueueEntry>,
}

/// 带 enqueue generation 的唯一 runqueue membership token。
pub(crate) struct RunQueueEntry {
    pub(crate) thread: Arc<Thread>,
    pub(crate) generation: u64,
    pub(crate) vruntime: u64,
}

impl CfsRunQueue {
    /// 创建尚未接管 backing 的空 Ready 队列。
    pub(crate) const fn new() -> Self {
        Self {
            tasks: PreallocatedHeap::new(),
        }
    }

    /// 在创建线程时为本地 Ready 队列准备替换存储。
    ///
    /// # Parameters
    ///
    /// - `capacity`: 本次准备接纳的存活线程数上界。
    ///
    /// # Returns
    ///
    /// 成功返回空 runqueue；heap OOM 返回错误。
    pub(crate) fn try_with_capacity(capacity: usize) -> Result<Self, ()> {
        Ok(Self {
            tasks: PreallocatedHeap::try_with_capacity(capacity)?,
        })
    }

    /// 是否能在当前 backing 中直接容纳新增条目。
    pub(crate) fn has_room(&self, additional: usize) -> bool {
        self.tasks.has_room(additional)
    }

    /// 在当前 backing 已满时接管更大的备用队列；没有备用存储时维持现状。
    pub(crate) fn adopt_spare(&mut self, spare: &mut Self) {
        if spare.has_room(1) {
            self.tasks.grow(core::mem::take(spare).tasks);
        }
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
    pub(crate) fn minimum_vruntime(&self) -> Option<u64> {
        self.tasks.peek().map(|entry| entry.vruntime)
    }
}

impl Default for CfsRunQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl PartialEq for RunQueueEntry {
    fn eq(&self, other: &Self) -> bool {
        self.vruntime == other.vruntime
            && self.generation == other.generation
            && self.thread.id() == other.thread.id()
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
        // 1. vruntime 小者优先；2. 线程 id/generation 形成稳定全序，避免 Ord 与 Eq 不一致。
        let by_vruntime = other.vruntime.cmp(&self.vruntime);
        by_vruntime
            .then_with(|| other.thread.id().cmp(&self.thread.id()))
            .then_with(|| other.generation.cmp(&self.generation))
    }
}
