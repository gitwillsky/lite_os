//! io_uring 的共享内存 ring：SQ 消费、CQ 发布、溢出链表与 readiness 通知。
//!
//! 所有权：ring 页由 `DeviceBacking` 持有并同时映射给用户态，内核与用户态按 Linux 约定分工 ——
//! SQ 的 tail 由用户态写、head 由内核写；CQ 的 tail 由内核写、head 由用户态写。head/tail 一律经
//! atomic 访问：内核先写 CQE 再 release-store tail，用户态 acquire-load tail 后才读 CQE；SQE 同理。

use alloc::{collections::VecDeque, sync::Arc};
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use spin::Mutex;

use super::abi::{
    CQ_HEAD, CQ_OVERFLOW, CQ_RING_ENTRIES, CQ_RING_MASK, CQ_TAIL, CQE_BYTES, CQES_OFFSET, Cqe,
    Geometry, SQ_CQ_OVERFLOW, SQ_DROPPED, SQ_FLAGS, SQ_HEAD, SQ_RING_ENTRIES, SQ_RING_MASK,
    SQ_TAIL, SQE_BYTES, Sqe,
};
use crate::{
    ipc::{Pipe, PipeEnd, PipeWaitCondition},
    memory::{DeviceBacking, FrameAllocationClass, PAGE_SIZE},
    sync::{TaskMutex, WaitResult},
};

/// 内核侧溢出链表相对 CQ 容量的倍数上限。超过后 CQE 被丢弃并计入 `cq_overflow`；缺少上限，
/// 一个不消费 CQ 的进程可以让内核无限增长。
const OVERFLOW_LIMIT_FACTOR: usize = 8;
const ZERO_CHUNK: usize = 256;

/// ring 创建失败原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RingError {
    OutOfMemory,
}

/// 逐个 SQE 的处理结果，决定 `submit` 是否继续消费后续条目。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Flow {
    Continue,
    /// 该 SQE 在 init/prep 阶段失败：Linux 在没有 `IORING_SETUP_SUBMIT_ALL` 时不再提交后续 SQE。
    Stop,
}

/// `wait_for` 的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CqWait {
    Satisfied,
    /// 有 CQE 因溢出链表上限被丢弃，且等待条件尚未满足（Linux 返回 `EBADR`）。
    Dropped,
    TimedOut,
    Interrupted,
    OutOfMemory,
}

struct CqState {
    // OWNER: 环已满时按序暂存的 CQE。新 CQE 在链表非空时必须排在链表尾，否则会越过更早的完成。
    overflow: VecDeque<Cqe>,
    // OWNER: 有 CQE 因链表上限被丢弃，尚未通过 `io_uring_enter` 的 `EBADR` 告知用户态。
    dropped_unreported: bool,
}

/// 一个 io_uring 实例的全部内核状态；同时是 ring fd 的 `DeviceFile` backend。
pub(crate) struct Ring {
    geometry: Geometry,
    ring: Arc<DeviceBacking>,
    sqes: Arc<DeviceBacking>,
    ring_identity: u64,
    sqes_identity: u64,
    // OWNER: 串行化 SQ 消费与 head 提交；缺失时两个线程同时 enter 会重复提交同一个 SQE。
    submit_lock: TaskMutex<()>,
    // OWNER: CQ 发布与溢出链表；缺失时并发 post 会覆盖同一个 CQE 槽。
    cq: Mutex<CqState>,
    // OWNER: 合并 readiness edge（read 端供 wait/epoll，write 端供发布者）。
    readiness: (Arc<PipeEnd>, Arc<PipeEnd>),
    // OWNER: epoll edge 判定用的全局 generation，每次 CQ 或 SQ 空间变化后推进。
    generation: AtomicU64,
    // OWNER: ring fd 的最后一个 OFD 引用已释放。挂起请求的执行者据此丢弃该 ring 的全部请求，
    // 释放它们持有的目标 OFD；缺失时关闭 ring 后，被挂起的 pipe 读端会一直保持打开，对端永远
    // 得不到 EPIPE/EOF。
    closed: AtomicBool,
    // OWNER: 已发布的 CQE 总数，以及其中由 timeout 完成的部分。带计数的 timeout 等待“除 timeout 自身外
    // 又发布了 N 个 CQE”（Linux `cached_cq_tail - cq_timeouts`）；缺失其一，两个 timeout 会互相计数而提前完成。
    posted: AtomicU32,
    timeouts_posted: AtomicU32,
}

fn allocate_zeroed(bytes: usize) -> Result<Arc<DeviceBacking>, RingError> {
    let backing = DeviceBacking::try_allocate(
        bytes.div_ceil(PAGE_SIZE).max(1),
        FrameAllocationClass::Reclaimable,
    )
    .ok_or(RingError::OutOfMemory)?;
    // 帧可能含旧内核数据；映射给用户态前必须清零，否则 ring 页泄露其他对象的残留字节。
    let zeros = [0u8; ZERO_CHUNK];
    for offset in (0..backing.pages() * PAGE_SIZE).step_by(ZERO_CHUNK) {
        backing
            .write(offset, &zeros)
            .map_err(|()| RingError::OutOfMemory)?;
    }
    Arc::try_new(backing).map_err(|_| RingError::OutOfMemory)
}

impl Ring {
    /// 分配并初始化 ring 页与 readiness 通知。
    ///
    /// # Errors
    ///
    /// 任一分配失败返回 `OutOfMemory`，不留下部分状态。
    pub(crate) fn try_new(geometry: Geometry) -> Result<Arc<Self>, RingError> {
        let ring = allocate_zeroed(geometry.ring_bytes)?;
        let sqes = allocate_zeroed(geometry.sqes_bytes)?;
        let readiness = Pipe::notification_pair().map_err(|()| RingError::OutOfMemory)?;
        let this = Arc::try_new(Self {
            geometry,
            ring,
            sqes,
            ring_identity: crate::id::next_runtime_object_id(),
            sqes_identity: crate::id::next_runtime_object_id(),
            submit_lock: TaskMutex::new(()),
            cq: Mutex::new(CqState {
                overflow: VecDeque::new(),
                dropped_unreported: false,
            }),
            readiness,
            generation: AtomicU64::new(crate::sync::next_readiness_generation()),
            closed: AtomicBool::new(false),
            posted: AtomicU32::new(0),
            timeouts_posted: AtomicU32::new(0),
        })
        .map_err(|_| RingError::OutOfMemory)?;
        this.word(SQ_RING_MASK)
            .store(geometry.sq_entries - 1, Ordering::Relaxed);
        this.word(CQ_RING_MASK)
            .store(geometry.cq_entries - 1, Ordering::Relaxed);
        this.word(SQ_RING_ENTRIES)
            .store(geometry.sq_entries, Ordering::Relaxed);
        this.word(CQ_RING_ENTRIES)
            .store(geometry.cq_entries, Ordering::Relaxed);
        Ok(this)
    }

    fn word(&self, offset: u32) -> &AtomicU32 {
        self.ring
            .atomic_u32(offset as usize)
            .expect("ring header offsets are inside the zero-initialised ring region")
    }

    /// 返回 mmap 偏移对应的 backing 与 futex identity；偏移必须精确等于 Linux 的三个魔数之一。
    pub(crate) fn region(&self, offset: u64) -> Option<(Arc<DeviceBacking>, u64)> {
        match offset {
            super::abi::OFF_SQ_RING | super::abi::OFF_CQ_RING => {
                Some((self.ring.clone(), self.ring_identity))
            }
            super::abi::OFF_SQES => Some((self.sqes.clone(), self.sqes_identity)),
            _ => None,
        }
    }

    /// 标记 ring 已关闭并唤醒所有等待者（含挂起请求的执行者）。
    pub(crate) fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.advance_generation();
        self.readiness.1.signal_readiness();
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// 发布一个请求的结果；`IOSQE_CQE_SKIP_SUCCESS` 的成功结果不产生 CQE。
    pub(crate) fn complete(&self, user_data: u64, skip_success: bool, res: i32) {
        if skip_success && res >= 0 {
            return;
        }
        self.post(Cqe {
            user_data,
            res,
            flags: 0,
        });
    }

    /// 发布 timeout 的完成；它不计入“事件数”，使其他带计数的 timeout 不会被它推进。
    pub(crate) fn complete_timeout(&self, user_data: u64, skip_success: bool, res: i32) {
        if skip_success && res >= 0 {
            return;
        }
        self.timeouts_posted.fetch_add(1, Ordering::AcqRel);
        self.post(Cqe {
            user_data,
            res,
            flags: 0,
        });
    }

    /// 自 ring 创建以来发布的非 timeout CQE 数（回绕算术）。
    pub(crate) fn events_posted(&self) -> u32 {
        self.posted
            .load(Ordering::Acquire)
            .wrapping_sub(self.timeouts_posted.load(Ordering::Acquire))
    }

    /// 提交线程与挂起请求执行者共用的唤醒 Pipe 读端，执行者把它作为 poll key 等待 ring 关闭。
    pub(crate) fn readiness_key(&self) -> &Arc<PipeEnd> {
        &self.readiness.0
    }

    fn advance_generation(&self) {
        self.generation
            .fetch_max(crate::sync::next_readiness_generation(), Ordering::AcqRel);
    }

    pub(crate) fn readiness_generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    pub(crate) fn readiness_read(&self) -> &Arc<PipeEnd> {
        &self.readiness.0
    }

    /// 用户态已发布、内核尚未消费的 SQE 数。
    pub(crate) fn sq_pending(&self) -> u32 {
        let tail = self.word(SQ_TAIL).load(Ordering::Acquire);
        tail.wrapping_sub(self.word(SQ_HEAD).load(Ordering::Relaxed))
    }

    /// CQ 环中用户态可消费的条目数（不含溢出链表）。
    pub(crate) fn cq_ready(&self) -> u32 {
        let tail = self.word(CQ_TAIL).load(Ordering::Relaxed);
        tail.wrapping_sub(self.word(CQ_HEAD).load(Ordering::Acquire))
    }

    /// 消费至多 `to_submit` 个 SQE，逐个交给 `execute`；返回被消费（计为已提交）的条目数。
    ///
    /// 1. 持 `submit_lock` 读取 tail（acquire），在 `min(to_submit, 可用)` 内逐个取 SQE。
    /// 2. SQ array 下标越界的条目计入 `sq_dropped`、仍然推进 head，但不计入返回值并停止，
    ///    与 Linux `io_get_sqe` 一致。
    /// 3. SQE 先整体复制出来（`IORING_FEAT_SUBMIT_STABLE`），之后用户态可立即复用该槽。
    /// 4. 全部处理完后一次 release-store head；缺失该顺序，用户态会过早复用仍在使用的 SQE 槽。
    pub(crate) fn submit(
        &self,
        to_submit: u32,
        mut execute: impl FnMut(Sqe) -> Flow,
    ) -> Result<u32, RingError> {
        let _guard = self
            .submit_lock
            .lock()
            .map_err(|_| RingError::OutOfMemory)?;
        let mask = self.geometry.sq_entries - 1;
        let mut head = self.word(SQ_HEAD).load(Ordering::Relaxed);
        let available = self
            .word(SQ_TAIL)
            .load(Ordering::Acquire)
            .wrapping_sub(head);
        let mut consumed = 0u32;
        for _ in 0..to_submit.min(available) {
            let slot = head & mask;
            head = head.wrapping_add(1);
            let array_offset = self.geometry.array_offset + slot * 4;
            let index = self
                .ring
                .atomic_u32(array_offset as usize)
                .expect("SQ array slot is inside the ring region")
                .load(Ordering::Relaxed);
            if index >= self.geometry.sq_entries {
                let dropped = self.word(SQ_DROPPED);
                dropped.store(
                    dropped.load(Ordering::Relaxed).wrapping_add(1),
                    Ordering::Relaxed,
                );
                break;
            }
            let mut raw = [0u8; SQE_BYTES];
            self.sqes
                .read(index as usize * SQE_BYTES, &mut raw)
                .expect("SQE index was bounds-checked against sq_entries");
            consumed += 1;
            if execute(Sqe::decode(&raw)) == Flow::Stop {
                break;
            }
        }
        self.word(SQ_HEAD).store(head, Ordering::Release);
        self.advance_generation();
        self.readiness.1.signal_readiness();
        Ok(consumed)
    }

    /// 发布一个 CQE：环有空位且溢出链表为空时直接入环，否则入溢出链表保持完成顺序。
    pub(crate) fn post(&self, cqe: Cqe) {
        self.posted.fetch_add(1, Ordering::AcqRel);
        let mut state = self.cq.lock();
        if state.overflow.is_empty() && self.try_publish(cqe) {
            drop(state);
        } else if state.overflow.len() < self.geometry.cq_entries as usize * OVERFLOW_LIMIT_FACTOR
            && state.overflow.try_reserve(1).is_ok()
        {
            state.overflow.push_back(cqe);
            self.word(SQ_FLAGS)
                .fetch_or(SQ_CQ_OVERFLOW, Ordering::Release);
            drop(state);
        } else {
            let counter = self.word(CQ_OVERFLOW);
            counter.store(
                counter.load(Ordering::Relaxed).wrapping_add(1),
                Ordering::Relaxed,
            );
            state.dropped_unreported = true;
            drop(state);
        }
        self.advance_generation();
        self.readiness.1.signal_readiness();
    }

    /// 环有空位时写入 CQE 并 release-store tail。
    fn try_publish(&self, cqe: Cqe) -> bool {
        let tail = self.word(CQ_TAIL).load(Ordering::Relaxed);
        let head = self.word(CQ_HEAD).load(Ordering::Acquire);
        if tail.wrapping_sub(head) >= self.geometry.cq_entries {
            return false;
        }
        let slot = tail & (self.geometry.cq_entries - 1);
        self.ring
            .write(
                CQES_OFFSET as usize + slot as usize * CQE_BYTES,
                &cqe.encode(),
            )
            .expect("CQE slot is inside the ring region");
        self.word(CQ_TAIL)
            .store(tail.wrapping_add(1), Ordering::Release);
        true
    }

    /// 把溢出链表尽可能搬回环；链表清空后撤销 `IORING_SQ_CQ_OVERFLOW`。
    pub(crate) fn flush_overflow(&self) {
        let mut state = self.cq.lock();
        let mut moved = false;
        while let Some(&front) = state.overflow.front() {
            if !self.try_publish(front) {
                break;
            }
            state.overflow.pop_front();
            moved = true;
        }
        if state.overflow.is_empty() {
            self.word(SQ_FLAGS)
                .fetch_and(!SQ_CQ_OVERFLOW, Ordering::Release);
        }
        drop(state);
        if moved {
            self.advance_generation();
        }
    }

    /// 取走“有 CQE 被丢弃”的一次性通知，对应 `io_uring_enter` 的 `EBADR`。
    pub(crate) fn take_dropped(&self) -> bool {
        core::mem::take(&mut self.cq.lock().dropped_unreported)
    }

    /// 环或溢出链表中是否有等待用户态的完成。
    pub(crate) fn has_completions(&self) -> bool {
        self.cq_ready() != 0 || !self.cq.lock().overflow.is_empty()
    }

    /// SQ 环是否还有空位（`POLLOUT`）。
    pub(crate) fn sq_has_space(&self) -> bool {
        self.sq_pending() < self.geometry.sq_entries
    }

    /// 阻塞直到 CQ 环至少有 `minimum` 个条目、`deadline`（绝对 monotonic ns）到期或收到 signal。
    ///
    /// 先排空 readiness token 再复查条件，再睡眠：排空与复查之间发布的 CQE 会重新置位 token，
    /// 因此不会丢唤醒。
    pub(crate) fn wait_for(&self, minimum: u32, deadline: Option<u64>) -> CqWait {
        loop {
            self.flush_overflow();
            if self.cq_ready() >= minimum {
                return CqWait::Satisfied;
            }
            if deadline.is_some_and(|value| value <= crate::timer::get_time_ns()) {
                return CqWait::TimedOut;
            }
            self.readiness.0.drain_readiness();
            self.flush_overflow();
            if self.cq_ready() >= minimum {
                return CqWait::Satisfied;
            }
            if self.take_dropped() {
                return CqWait::Dropped;
            }
            match self
                .readiness
                .0
                .pipe()
                .wait(PipeWaitCondition::Readable, deadline)
            {
                WaitResult::Woken | WaitResult::TimedOut => {}
                WaitResult::Interrupted => return CqWait::Interrupted,
                WaitResult::OutOfMemory => return CqWait::OutOfMemory,
            }
        }
    }
}
