//! 唯一 Thread/Process pending owner；普通信号保留首个来源，实时信号按号 FIFO。
use super::{PendingSignal, signal_info::SignalFallback};
use alloc::{boxed::Box, sync::Arc};
use core::ptr::NonNull;
use spin::Mutex;

const REALTIME_MIN: usize = 32;

/// 唯一实际信号 queue record；动态收费或 timer 预留 lifetime 由 source 独占。
#[derive(Debug)]
struct SignalRecord {
    info: PendingSignal,
    source: SignalRecordSource,
    next: Option<Box<SignalRecord>>,
}

/// 动态信号独占收费；预分配节点排队期间持有实际 timer owner，空闲时不形成自环。
#[derive(Debug)]
enum SignalRecordSource {
    Dynamic {
        _charge: crate::namespace::RlimitCharge,
    },
    Timer(Option<Arc<PreallocatedSignal>>),
}

#[derive(Debug)]
struct PreallocatedSignalSlot {
    available: Option<Box<SignalRecord>>,
    queued_sequence: u64,
    key: (usize, i32),
}

/// POSIX timer 创建时的唯一预留节点及 ucounts owner；queued observer 保留原收费 lifetime。
/// slot 锁只移动实际 Box，不分配、不进入 pending/graph/timer registry。
#[derive(Debug)]
pub(in crate::execution::task) struct PreallocatedSignal {
    slot: Mutex<PreallocatedSignalSlot>,
    _charge: crate::namespace::RlimitCharge,
}

/// 离开 pending guard 后交给 timer coordinator 的真实队列来源。
pub(in crate::execution::task) struct TimerSignalDelivery {
    pub(in crate::execution::task) owner: Arc<PreallocatedSignal>,
    pub(in crate::execution::task) sequence: u64,
    pub(in crate::execution::task) key: (usize, i32),
}
impl PreallocatedSignal {
    /// 创建一个 POSIX timer 的实际预留节点，以 creator real UID/UserNS 和 soft limit 收费。
    /// 包括 SIGEV_NONE；配额/Box/Arc preparation 失败返回 Err，调用方映射 Linux EAGAIN。
    pub(in crate::execution::task) fn try_new(
        namespace: Arc<crate::namespace::UserNamespace>,
        uid: u32,
        limit: u64,
        key: (usize, i32),
    ) -> Result<Arc<Self>, ()> {
        let charge = crate::namespace::RlimitCharge::acquire(
            namespace,
            uid,
            crate::namespace::RlimitResource::PendingSignals,
            1,
            limit,
            false,
        )
        .map_err(|_| ())?;
        let record = Box::try_new(SignalRecord {
            info: PendingSignal::default(),
            source: SignalRecordSource::Timer(None),
            next: None,
        })
        .map_err(|_| ())?;
        Arc::try_new(Self {
            slot: Mutex::new(PreallocatedSignalSlot {
                available: Some(record),
                queued_sequence: 0,
                key,
            }),
            _charge: charge,
        })
        .map_err(|_| ())
    }
    /// 仅 unpublished creation retry 可更改候选 key；已排队节点不得绑定新 ID lifetime。
    pub(in crate::execution::task) fn retarget(&self, key: (usize, i32)) {
        let mut slot = self.slot.lock();
        assert!(slot.available.is_some());
        slot.key = key;
    }
    /// 每次实际 expiry 更新 queued sequence；已有节点排队时不再生成第二个通知。
    pub(in crate::execution::task) fn prepare(
        self: &Arc<Self>,
        sequence: u64,
    ) -> PreparedSignalRecord {
        let mut slot = self.slot.lock();
        slot.queued_sequence = sequence;
        let record = slot.available.take().map(|mut record| {
            record.source = SignalRecordSource::Timer(Some(self.clone()));
            record
        });
        PreparedSignalRecord { record }
    }
}
impl SignalRecord {
    fn timer_delivery(&self) -> Option<TimerSignalDelivery> {
        let SignalRecordSource::Timer(Some(owner)) = &self.source else {
            return None;
        };
        let slot = owner.slot.lock();
        Some(TimerSignalDelivery {
            owner: owner.clone(),
            sequence: slot.queued_sequence,
            key: slot.key,
        })
    }
    /// 被取出的真实 Box 先归还 slot，随后 coordinator 才重装 deadline，避免即时 expiry 丢失。
    fn retire(mut record: Box<Self>) -> Option<TimerSignalDelivery> {
        let SignalRecordSource::Timer(owner) = &mut record.source else {
            drop(record);
            return None;
        };
        let Some(owner) = owner.take() else {
            drop(record);
            return None;
        };
        let (sequence, key) = {
            let mut slot = owner.slot.lock();
            assert!(slot.available.is_none());
            let delivery = (slot.queued_sequence, slot.key);
            slot.available = Some(record);
            delivery
        };
        Some(TimerSignalDelivery {
            owner,
            sequence,
            key,
        })
    }
}

/// 单一 owned 链；tail 只借用链内最后一个节点，不拥有另一份 membership。
#[derive(Debug, Default)]
pub(in crate::execution::task) struct RetiredSignals {
    head: Option<Box<SignalRecord>>,
    tail: Option<NonNull<SignalRecord>>,
}
// SAFETY: tail 始终指向本容器独占的 Box，移动容器不移动 allocation；只经 &mut/self owner guard
// 解引用。不对外提供引用或裸指针，跨线程移动不会产生共享可变访问。
unsafe impl Send for RetiredSignals {}
impl RetiredSignals {
    fn push(&mut self, mut record: Box<SignalRecord>) {
        assert!(record.next.is_none());
        let tail = NonNull::from(record.as_mut());
        if let Some(mut previous) = self.tail {
            // SAFETY: previous 是独占链内的 live 最后 Box；追加不移动、不释放 previous。
            unsafe { previous.as_mut().next = Some(record) };
        } else {
            self.head = Some(record);
        }
        self.tail = Some(tail);
    }
    fn pop(&mut self) -> Option<Box<SignalRecord>> {
        let mut record = self.head.take()?;
        self.head = record.next.take();
        if self.head.is_none() {
            self.tail = None;
        }
        Some(record)
    }
    /// SIG_IGN coordinator 持 graph 且 predicate 只接受 registry 内的周期 timer。
    /// 接受项归还其真实预留 Box；无效/单次项保留到锁外退休。
    pub(in crate::execution::task) fn recycle_timers(
        &mut self,
        mut accept: impl FnMut(&TimerSignalDelivery) -> bool,
    ) {
        let mut remaining = Self::default();
        while let Some(record) = self.pop() {
            let accepted = record.timer_delivery().as_ref().is_some_and(&mut accept);
            if accepted {
                drop(SignalRecord::retire(record));
            } else {
                remaining.push(record);
            }
        }
        self.append(remaining);
    }
    fn extract_timer_notifications(&mut self) -> Self {
        let mut timers = Self::default();
        let mut remaining = Self::default();
        while let Some(record) = self.pop() {
            if record.info.is_timer_notification() {
                timers.push(record);
            } else {
                remaining.push(record);
            }
        }
        self.append(remaining);
        timers
    }
    pub(in crate::execution::task) fn append(&mut self, mut other: Self) {
        if other.head.is_none() {
            return;
        }
        if let Some(mut tail) = self.tail {
            // SAFETY: 两个容器互不重叠且都独占，splice 只写旧尾的 next；other 随后失去链。
            unsafe { tail.as_mut().next = other.head.take() };
        } else {
            self.head = other.head.take();
        }
        self.tail = other.tail.take();
    }
}
impl Drop for RetiredSignals {
    fn drop(&mut self) {
        // 不递归 Box.next：大队列的丢弃不能耗尽 kernel stack。
        while let Some(record) = self.pop() {
            drop(SignalRecord::retire(record));
        }
    }
}

/// generation 的锁外 preparation；没有 storage 表示 OOM/限额，commit 按来源决定失败或兜底。
#[derive(Default)]
pub(in crate::execution::task) struct PreparedSignalRecord {
    record: Option<Box<SignalRecord>>,
}
impl PreparedSignalRecord {
    pub(in crate::execution::task) fn prepare(
        signal: usize,
        info: PendingSignal,
        namespace: alloc::sync::Arc<crate::namespace::UserNamespace>,
        uid: u32,
        limit: u64,
    ) -> Self {
        let record = (|| {
            if !(1..=64).contains(&signal) || signal == syscall_abi::signal::SIGKILL {
                return None;
            }
            let charge = crate::namespace::RlimitCharge::acquire(
                namespace,
                uid,
                crate::namespace::RlimitResource::PendingSignals,
                1,
                limit,
                info.overrides_pending_limit(signal),
            )
            .ok()?;
            Box::try_new(SignalRecord {
                info: PendingSignal::default(),
                source: SignalRecordSource::Dynamic { _charge: charge },
                next: None,
            })
            .ok()
        })();
        Self { record }
    }
    pub(in crate::execution::task) fn has_record(&self) -> bool {
        self.record.is_some()
    }
    fn is_timer(&self) -> bool {
        self.record
            .as_ref()
            .is_some_and(|record| matches!(record.source, SignalRecordSource::Timer(_)))
    }
    /// coordinator 保证 registry 仍持有 owner，故归还不会在 graph 内释放配额或 storage。
    pub(in crate::execution::task) fn recycle_timer(&mut self) {
        if let Some(record) = self.record.take() {
            drop(SignalRecord::retire(record));
        }
    }
    pub(super) fn unavailable_for(&self, signal: usize, info: PendingSignal) -> bool {
        signal >= REALTIME_MIN && self.record.is_none() && !info.allows_queue_fallback()
    }
}

/// 从 pending owner 摘除的实际 record；调用者必须移出所有 owner/graph guard 后再解包。
pub(super) struct TakenSignal {
    pub(super) signal: usize,
    info: PendingSignal,
    record: Option<Box<SignalRecord>>,
}
impl TakenSignal {
    /// 释放 pending guards 后消费：先归还预留 Box，再执行真实 timer lifetime/sequence 验证。
    /// coordinator 返回 false 时丢弃旧通知；普通节点直接退款并返回首个 siginfo。
    pub(super) fn resolve(
        mut self,
        timer: impl FnOnce(TimerSignalDelivery, &mut PendingSignal) -> bool,
    ) -> Option<(usize, PendingSignal)> {
        let event = self.record.take().and_then(SignalRecord::retire);
        if event.is_some_and(|event| !timer(event, &mut self.info)) {
            return None;
        }
        Some(self.into_parts())
    }
    pub(super) fn into_parts(self) -> (usize, PendingSignal) {
        (self.signal, self.info)
    }
}

impl Drop for PreparedSignalRecord {
    fn drop(&mut self) {
        if let Some(record) = self.record.take() {
            drop(SignalRecord::retire(record));
        }
    }
}
impl Drop for TakenSignal {
    fn drop(&mut self) {
        if let Some(record) = self.record.take() {
            drop(SignalRecord::retire(record));
        }
    }
}

#[derive(Debug)]
pub(super) struct PendingSignals {
    pub(super) bits: u64,
    fallback: [SignalFallback; 65],
    records: [RetiredSignals; 64],
}
// LP64 pending owner 不内嵌历史 payload 表；有 payload 的事件只在真实 queue record 中。
const _: () = assert!(core::mem::size_of::<PendingSignals>() <= 1152);
impl PendingSignals {
    pub(super) fn new() -> Self {
        Self {
            bits: 0,
            fallback: [SignalFallback::NONE; 65],
            records: core::array::from_fn(|_| RetiredSignals::default()),
        }
    }
    pub(super) fn queue(
        &mut self,
        signal: usize,
        info: PendingSignal,
        prepared: &mut PreparedSignalRecord,
    ) {
        let bit = 1u64 << (signal - 1);
        if signal < REALTIME_MIN && !prepared.is_timer() && self.bits & bit != 0 {
            if let Some(record) = self.records[signal - 1].head.as_mut() {
                record.info.merge_force(info);
            } else {
                self.fallback[signal].merge_force(info);
            }
            return;
        }
        if let Some(mut record) = prepared.record.take() {
            record.info = info;
            if signal < REALTIME_MIN
                && self.bits & bit != 0
                && self.records[signal - 1].head.is_none()
            {
                record.info.merge_force(self.fallback[signal].info());
            }
            self.records[signal - 1].push(record);
        } else if self.bits & bit == 0 {
            // 原生 sigqueue OOM/fast path：只有 pending bit；visible siginfo 清零，保留内核 force consequence。
            self.fallback[signal] = SignalFallback::from_info(info);
        }
        self.bits |= bit;
    }
    /// 首个真实来源保留不可丢失的 delivery consequence；不创建平行 pending mask。
    pub(super) fn requires_delivery(&self, mut mask: u64) -> bool {
        mask &= self.bits;
        while mask != 0 {
            let index = mask.trailing_zeros() as usize;
            mask &= mask - 1;
            let info = self.records[index]
                .head
                .as_ref()
                .map_or_else(|| self.fallback[index + 1].info(), |record| record.info);
            if info.requires_delivery() {
                return true;
            }
        }
        false
    }
    pub(super) fn take(&mut self, mask: u64) -> Option<TakenSignal> {
        let available = self.bits & mask;
        if available == 0 {
            return None;
        }
        let signal = available.trailing_zeros() as usize + 1;
        let record = self.records[signal - 1].pop();
        if self.records[signal - 1].head.is_none() {
            self.bits &= !(1u64 << (signal - 1));
        }
        Some(TakenSignal {
            signal,
            info: record
                .as_ref()
                .map_or_else(|| self.fallback[signal].info(), |record| record.info),
            record,
        })
    }
    /// exec 对照 Linux flush_itimer_signals，只摘 SI_TIMER record，保留同号其他真实来源。
    pub(super) fn discard_timer_notifications(&mut self) -> RetiredSignals {
        let mut retired = RetiredSignals::default();
        for signal in 1..=64 {
            let timers = self.records[signal - 1].extract_timer_notifications();
            if self.records[signal - 1].head.is_none() && timers.head.is_some() {
                self.bits &= !(1u64 << (signal - 1));
            }
            retired.append(timers);
        }
        retired
    }
    pub(super) fn discard(&mut self, mask: u64) -> RetiredSignals {
        self.bits &= !mask;
        let mut retired = RetiredSignals::default();
        if mask == 0 {
            return retired;
        }
        for signal in 1..=64 {
            if mask & (1u64 << (signal - 1)) != 0 {
                retired.append(core::mem::take(&mut self.records[signal - 1]));
            }
        }
        retired
    }
}
