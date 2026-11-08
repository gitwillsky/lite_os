//! 每 CPU 的公平/EDF/预算补充队列；过期 generation 惰性删除，稳态入队不分配。
//!
//! # 它解决什么问题
//!
//! 完全公平调度（CFS）的想法只有一句：每个任务累计一个“虚拟运行时间” vruntime，调度器总是挑
//! vruntime 最小的任务运行——跑得少的先跑，长期看每个任务得到的 CPU 份额与它的权重成正比。
//!
//! # vruntime 怎么来
//!
//! 任务每结束一段 CPU 时间片，就把运行时间按权重等级对应的权重折算后加到 vruntime 上（默认权重时
//! 1 ns 对应 1 个单位，权重越大涨得越慢，见 `thread/scheduling.rs`）。新建任务放置到某个
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
//! 可运行 EDF 优先于公平类；类内键严格更早才请求唤醒抢占，相等不抢。无预算者等待补充，不能压住公平任务。
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

use super::deadline::ReadyKey;

/// 每 CPU 三个有序集合：公平、可运行 EDF、等待补充的 EDF。所有 backing 在线程接纳时预留。
pub(crate) struct RunQueue {
    fair: PreallocatedHeap<RunQueueEntry>,
    deadline: PreallocatedHeap<RunQueueEntry>,
    throttled: PreallocatedHeap<Throttled>,
}

/// 带 enqueue generation 的唯一 Ready membership；策略替换推进 generation，迟到 token 无效。
pub(crate) struct RunQueueEntry {
    pub(crate) thread: Arc<Thread>,
    pub(crate) generation: u64,
    pub(crate) key: ReadyKey,
}
struct Throttled(RunQueueEntry);
impl Throttled {
    fn time(&self) -> u64 {
        match self.0.key {
            ReadyKey::Deadline {
                not_before: Some(time),
                ..
            } => time,
            _ => panic!("unthrottled entity in replenishment queue"),
        }
    }
}
impl PartialEq for Throttled {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}
impl Eq for Throttled {}
impl Ord for Throttled {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .time()
            .cmp(&self.time())
            .then_with(|| self.0.cmp(&other.0))
    }
}
impl PartialOrd for Throttled {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl RunQueue {
    pub(crate) const fn new() -> Self {
        Self {
            fair: PreallocatedHeap::new(),
            deadline: PreallocatedHeap::new(),
            throttled: PreallocatedHeap::new(),
        }
    }
    /// 线程创建可失败阶段准备全部排序集合，切换/补充路径不分配。
    pub(crate) fn try_with_capacity(capacity: usize) -> Result<Self, ()> {
        Ok(Self {
            fair: PreallocatedHeap::try_with_capacity(capacity)?,
            deadline: PreallocatedHeap::try_with_capacity(capacity)?,
            throttled: PreallocatedHeap::try_with_capacity(capacity)?,
        })
    }
    pub(crate) fn has_room(&self, additional: usize) -> bool {
        self.fair.has_room(additional)
            && self.deadline.has_room(additional)
            && self.throttled.has_room(additional)
    }
    pub(crate) fn adopt_spare(&mut self, spare: &mut Self) {
        if spare.has_room(1) {
            let spare = core::mem::take(spare);
            self.fair.grow(spare.fair);
            self.deadline.grow(spare.deadline);
            self.throttled.grow(spare.throttled);
        }
    }
    /// 只在容量不足时清理失效 token；三个集合各自容量可容纳全部存活线程。
    pub(crate) fn make_room(
        &mut self,
        additional: usize,
        mut keep: impl FnMut(&RunQueueEntry) -> bool,
    ) -> usize {
        self.fair.make_room(additional, &mut keep)
            + self.deadline.make_room(additional, &mut keep)
            + self.throttled.make_room(additional, |entry| keep(&entry.0))
    }
    pub(crate) fn discard_stale_roots(
        &mut self,
        mut keep: impl FnMut(&RunQueueEntry) -> bool,
    ) -> usize {
        self.fair.discard_invalid_roots(&mut keep)
            + self.deadline.discard_invalid_roots(&mut keep)
            + self.throttled.discard_invalid_roots(|entry| keep(&entry.0))
    }
    pub(crate) fn push(&mut self, entry: RunQueueEntry) {
        match entry.key {
            ReadyKey::Fair(_) => self.fair.push(entry),
            ReadyKey::Deadline {
                not_before: None, ..
            } => self.deadline.push(entry),
            ReadyKey::Deadline {
                not_before: Some(_),
                ..
            } => self.throttled.push(Throttled(entry)),
        }
    }
    pub(crate) fn pop(&mut self) -> Option<RunQueueEntry> {
        self.deadline.pop().or_else(|| self.fair.pop())
    }
    pub(crate) fn take_due(&mut self, now: u64) -> Option<RunQueueEntry> {
        if self
            .throttled
            .peek()
            .is_some_and(|entry| entry.time() <= now)
        {
            self.throttled.pop().map(|entry| entry.0)
        } else {
            None
        }
    }
    pub(crate) fn next_replenishment(&self) -> Option<u64> {
        self.throttled.peek().map(Throttled::time)
    }
    pub(crate) fn has_eligible(&self) -> bool {
        self.deadline.peek().is_some() || self.fair.peek().is_some()
    }
    /// CFS 虚拟时钟不能被 EDF 的真实纳秒截止推进。
    pub(crate) fn minimum_vruntime(&self) -> Option<u64> {
        self.fair.peek().map(|entry| match entry.key {
            ReadyKey::Fair(vruntime) => vruntime,
            _ => unreachable!(),
        })
    }
}
impl Default for RunQueue {
    fn default() -> Self {
        Self::new()
    }
}
impl PartialEq for RunQueueEntry {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
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
        other
            .key
            .cmp(&self.key)
            .then_with(|| other.thread.id().cmp(&self.thread.id()))
            .then_with(|| other.generation.cmp(&self.generation))
    }
}
