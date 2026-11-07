//! Request slot、descriptor credits、completion identity 与 FIFO capacity wait 的共同 owner。

use crate::sync::Mutex;
use alloc::{sync::Arc, vec::Vec};

use crate::io_completion::{IoCompletion, IoDevice, IoWaitKey, IoWaitTarget};
use fallible_tree::{FallibleMap, VacantEntry};

/// request slot acquisition 的 adapter-independent failure。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RequestOwnerError {
    /// waiter metadata allocation failed。
    OutOfMemory,
    /// device 已进入 terminal failure。
    DeviceFailed,
}

/// descriptor head 映射到的固定 slot generation identity。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RequestIdentity {
    /// request 使用的固定 slot 下标。
    pub(crate) slot: u16,
    /// 区分同一 slot 连续 request 的单调 generation。
    pub(crate) generation: u64,
}

/// 从 descriptor index 暂时摘取、尚待 adapter 验证的 completion identity。
///
/// Token 必须 exactly once 传给 `RequestOwner::accept_completion` 或
/// `RequestOwner::reject_completion`；静默丢弃会让 request 脱离 failure drain 并永久丢 wake。
#[must_use = "completion claim must be accepted or rejected exactly once"]
pub(crate) struct CompletionClaim {
    head: u16,
    identity: RequestIdentity,
}

impl CompletionClaim {
    /// 在 adapter validation 期间读取 claimed request identity。
    ///
    /// # Returns
    ///
    /// descriptor 原先映射的完整 slot/generation identity。
    pub(crate) fn identity(&self) -> RequestIdentity {
        self.identity
    }
}

/// 容量结果的消费者：线程握手或 reactor 通知；两者共用同一个 FIFO/permit owner。
#[derive(Clone)]
pub(crate) enum CapacityTarget {
    Thread(Arc<dyn IoWaitTarget>),
    Event(Arc<native_runtime::Event>),
}

/// slot capacity waiter、completion handshake 与 handoff result 的唯一 owner。
pub(crate) struct CapacityWait {
    key: IoWaitKey,
    descriptors: usize,
    completion: IoCompletion,
    target: CapacityTarget,
    // OWNER: wait node 独占单次 slot handoff/terminal failure result。缺失该 result 会使
    // release/reset race 唤醒一个没有 permit 的 caller。
    outcome: Mutex<Option<Result<RequestIdentity, RequestOwnerError>>>,
}

impl CapacityWait {
    fn new(key: IoWaitKey, descriptors: usize, target: CapacityTarget) -> Self {
        let completion = IoCompletion::new();
        completion.reset();
        Self {
            key,
            descriptors,
            completion,
            target,
            outcome: Mutex::new(None),
        }
    }

    /// 在发布时记录的等待目标上睡眠，直到 slot handoff 或 terminal failure。
    ///
    /// 结果由 `take_outcome` 消费。
    pub(crate) fn wait(&self) {
        let CapacityTarget::Thread(target) = &self.target else {
            panic!("reactor capacity waiter must be polled")
        };
        target.clone().sleep(&self.completion, self.key);
    }

    /// 消费 wake 之前发布的唯一 capacity outcome。
    ///
    /// # Returns
    ///
    /// 获得 handoff slot 时返回 identity，terminal failure 时返回 device error。
    ///
    /// # Errors
    ///
    /// device reset/failure 返回 `DeviceFailed`；重复消费或无 outcome 时 fail-stop。
    pub(crate) fn take_outcome(&self) -> Result<RequestIdentity, RequestOwnerError> {
        self.try_take_outcome()
            .expect("driver capacity waiter woke without outcome")
    }

    /// reactor 消费已发表结果；尚未 handoff 时返回 None，不等待线程。
    pub(crate) fn try_take_outcome(&self) -> Option<Result<RequestIdentity, RequestOwnerError>> {
        self.outcome.lock().take()
    }

    /// 在摘除 waiter 后发布 slot handoff 或 terminal failure outcome。
    ///
    /// # Parameters
    ///
    /// - `outcome`: 唯一 slot identity 或 terminal device error。
    ///
    /// # Returns
    ///
    /// Thread 已武装时返回锁外 wake，completion-before-arm 返回 None；Event 总是返回锁外通知。
    ///
    /// # Errors
    ///
    /// 不返回错误；重复发布同一 waiter 时 fail-stop。
    pub(crate) fn publish(
        &self,
        outcome: Result<RequestIdentity, RequestOwnerError>,
    ) -> Option<CapacityWake> {
        assert!(self.outcome.lock().replace(outcome).is_none());
        let armed = self.completion.complete();
        (armed || matches!(&self.target, CapacityTarget::Event(_))).then(|| CapacityWake {
            target: self.target.clone(),
            request: self.key,
        })
    }
}

/// owner 锁外消费 scheduler capacity membership 的唯一 wake capability。
pub(crate) struct CapacityWake {
    target: CapacityTarget,
    request: IoWaitKey,
}

impl CapacityWake {
    /// 唤醒持有精确 capacity wait key 的 task。
    ///
    /// # Errors
    ///
    /// 不返回错误；scheduler membership mismatch 由 target fail-stop。
    pub(crate) fn wake(self) {
        match self.target {
            CapacityTarget::Thread(target) => target.wake(self.request),
            CapacityTarget::Event(event) => event
                .signal(0, native_runtime::native_abi::signals::SIGNALED)
                .expect("private capacity Event must remain usable"),
        }
    }

    /// 在 owner 锁外唤醒已授予的等待者，再逐个取得剩余可满足的 FIFO handoff。
    ///
    /// 不分配 wake 数组；`next` 只在前一个 wake 已完成后调用，并负责检查 adapter 是否失效。
    pub(crate) fn drain(mut wake: Option<Self>, mut next: impl FnMut() -> Option<Self>) {
        while let Some(current) = wake {
            current.wake();
            wake = next();
        }
    }
}

/// owner 锁外预分配、尚未发布的 capacity waiter 与 FIFO index node。
pub(crate) struct PreparedCapacityWait {
    waiter: Arc<CapacityWait>,
    entry: VacantEntry<u64, Arc<CapacityWait>>,
}

impl PreparedCapacityWait {
    /// 为一次 capacity publication 预分配 waiter 与 FIFO node。
    ///
    /// # Parameters
    ///
    /// - `key`: owner 已签发的 capacity wait key。
    /// - `descriptors`: 本次请求实际需要的 descriptor 数；与首次 reserve 参数一致。
    /// - `target`: 调用线程的等待目标或 reactor 的 Event；Event 通知不要求先睡眠。
    ///
    /// # Returns
    ///
    /// 完整 prepared owner，供 `commit_wait_or_reserve` 无分配提交。
    ///
    /// # Errors
    ///
    /// Arc 或 ordered-index node 分配失败返回 `OutOfMemory`；非 capacity key fail-stop。
    pub(crate) fn try_new(
        key: IoWaitKey,
        descriptors: usize,
        target: CapacityTarget,
    ) -> Result<Self, RequestOwnerError> {
        let waiter = Arc::try_new(CapacityWait::new(key, descriptors, target))
            .map_err(|_| RequestOwnerError::OutOfMemory)?;
        let crate::io_completion::IoWaitKind::Capacity(ticket) = key.kind else {
            panic!("request key used for capacity wait")
        };
        let entry = FallibleMap::try_prepare(ticket, waiter.clone())
            .map_err(|_| RequestOwnerError::OutOfMemory)?;
        Ok(Self { waiter, entry })
    }
}

/// slot 首次预留的无分配决策。
pub(crate) enum ReserveOrWait {
    /// 已取得唯一 slot identity。
    Reserved(RequestIdentity),
    /// capacity 已满；携带待在锁外预分配 waiter 的 FIFO ticket。
    Prepare(u64),
}

/// prepared waiter 重入 owner 后的最终 publication 决策。
pub(crate) enum CommitOrWait {
    /// prepare window 内释放了 slot，prepared waiter 不发布并直接取得 identity。
    Reserved(RequestIdentity),
    /// capacity 仍满，waiter 已进入 FIFO index，caller 必须等待 outcome。
    Waiting(Arc<CapacityWait>),
}

/// 一个 slot 的唯一预留记录；generation 为零表示空闲，其余字段不在 adapter 中复制。
struct SlotState {
    generation: u64,
    descriptors: u32,
    next_free: Option<u16>,
}

/// 固定 slot lifecycle、descriptor-head projection 与 FIFO capacity membership。
pub(crate) struct RequestOwner {
    by_head: Vec<Option<RequestIdentity>>,
    slots: Vec<SlotState>,
    free_slot: Option<u16>,
    // OWNER: 与 slot 预留同一事务扣除/归还；不从尚未发布的 descriptor 链推算容量。
    // 缺失时多个已取得 slot 的 scatter-gather 请求可在 publication 时争抢同一份描述符容量。
    available_descriptors: usize,
    next_generation: u64,
    // OWNER: 本对象是 capacity membership 的唯一 FIFO index；release 必须先摘除再 wake。
    capacity_waiters: FallibleMap<u64, Arc<CapacityWait>>,
    next_capacity_ticket: u64,
    device: IoDevice,
}

impl RequestOwner {
    /// 构造固定 request slots、descriptor projection 与空 capacity FIFO。
    ///
    /// # Parameters
    ///
    /// - `queue_size`: VirtQueue descriptor head 的可寻址数量。
    /// - `slots`: adapter 固定 request slot 数量。
    /// - `device`: scheduler wait key 中的 adapter identity。
    ///
    /// # Returns
    ///
    /// 全部 storage 预留成功时返回 owner，否则返回 `None`。
    ///
    /// # Errors
    ///
    /// slot/descriptor 数量不可表示或 storage 分配失败以 `None` 表示，不发布部分 owner。
    pub(crate) fn new(queue_size: usize, slots: usize, device: IoDevice) -> Option<Self> {
        if slots == 0 || slots > queue_size || queue_size > usize::from(u16::MAX) + 1 {
            return None;
        }
        let mut by_head = Vec::new();
        by_head.try_reserve_exact(queue_size).ok()?;
        by_head.resize(queue_size, None);
        let mut states = Vec::new();
        states.try_reserve_exact(slots).ok()?;
        states.extend((0..slots).map(|slot| SlotState {
            generation: 0,
            descriptors: 0,
            next_free: (slot + 1 < slots).then_some((slot + 1) as u16),
        }));
        Some(Self {
            by_head,
            slots: states,
            free_slot: Some(0),
            available_descriptors: queue_size,
            next_generation: 1,
            capacity_waiters: FallibleMap::new(),
            next_capacity_ticket: 1,
            device,
        })
    }

    fn reserve(&mut self, descriptors: usize) -> Option<RequestIdentity> {
        assert!(
            descriptors != 0 && descriptors <= self.by_head.len(),
            "invalid request descriptor footprint"
        );
        if descriptors > self.available_descriptors {
            return None;
        }
        let slot = self.free_slot?;
        let generation = self.next_generation;
        self.next_generation = generation
            .checked_add(1)
            .expect("driver request generation exhausted");
        let state = &mut self.slots[slot as usize];
        assert_eq!(state.generation, 0, "reserved slot remains on free list");
        self.free_slot = state.next_free.take();
        state.generation = generation;
        state.descriptors = descriptors as u32;
        self.available_descriptors -= descriptors;
        Some(RequestIdentity { slot, generation })
    }

    /// 原子预留空闲 slot 与 `descriptors` 个 credit，或签发锁外 waiter preparation ticket。
    ///
    /// # Returns
    ///
    /// 有容量时返回唯一 identity，否则返回尚未发布的 FIFO ticket。
    ///
    /// # Errors
    ///
    /// 不返回错误；generation 或 ticket 耗尽时 fail-stop。
    pub(crate) fn reserve_or_wait(&mut self, descriptors: usize) -> ReserveOrWait {
        assert!(
            descriptors != 0 && descriptors <= self.by_head.len(),
            "invalid request descriptor footprint"
        );
        if self.capacity_waiters.is_empty()
            && let Some(identity) = self.reserve(descriptors)
        {
            return ReserveOrWait::Reserved(identity);
        }
        let ticket = self.next_capacity_ticket;
        self.next_capacity_ticket = ticket
            .checked_add(1)
            .expect("driver capacity wait ticket exhausted");
        ReserveOrWait::Prepare(ticket)
    }

    /// 把 owner 签发的 FIFO ticket 编码为 typed scheduler key。
    ///
    /// # Parameters
    ///
    /// - `ticket`: `reserve_or_wait` 返回的完整 ticket。
    ///
    /// # Returns
    ///
    /// 保留 device identity 的 capacity wait key。
    pub(crate) fn capacity_key(&self, ticket: u64) -> IoWaitKey {
        IoWaitKey::capacity(self.device, ticket)
    }

    /// prepare window 后复查 capacity，并原子提交 waiter 或直接预留 slot。
    ///
    /// # Parameters
    ///
    /// - `prepared`: 锁外完成全部可失败分配的 capacity waiter owner。
    ///
    /// # Returns
    ///
    /// 新出现容量时返回 identity，否则返回已发布 waiter。
    ///
    /// # Errors
    ///
    /// 不返回错误；prepared ticket 重复时由 ordered index fail-stop。
    pub(crate) fn commit_wait_or_reserve(
        &mut self,
        prepared: PreparedCapacityWait,
    ) -> CommitOrWait {
        assert!(
            prepared.waiter.descriptors != 0 && prepared.waiter.descriptors <= self.by_head.len(),
            "invalid prepared descriptor footprint"
        );
        let first_in_fifo = self
            .capacity_waiters
            .first_key_value()
            .is_none_or(|(ticket, _)| prepared.entry.key() < ticket);
        if first_in_fifo && let Some(identity) = self.reserve(prepared.waiter.descriptors) {
            return CommitOrWait::Reserved(identity);
        }
        let PreparedCapacityWait { waiter, entry } = prepared;
        self.capacity_waiters.commit_vacant(entry);
        CommitOrWait::Waiting(waiter)
    }

    /// 在 descriptor 成功提交后发布 head 到 request identity 的唯一 projection。
    ///
    /// # Parameters
    ///
    /// - `head`: 已发布 descriptor chain 的 head index。
    /// - `identity`: descriptor 使用的固定 slot/generation identity。
    ///
    /// # Errors
    ///
    /// head 越界或覆盖 live mapping 时 fail-stop。
    pub(crate) fn publish(&mut self, head: u16, identity: RequestIdentity) {
        let state = &self.slots[identity.slot as usize];
        assert_ne!(state.generation, 0, "publication without reserved slot");
        assert_eq!(
            state.generation, identity.generation,
            "stale request publication"
        );
        assert!(self.by_head[head as usize].replace(identity).is_none());
    }

    /// 暂时摘取 used descriptor 对应 identity，交给 adapter 验证。
    ///
    /// # Parameters
    ///
    /// - `head`: device 返回的 descriptor head。
    ///
    /// # Returns
    ///
    /// live mapping 存在时返回必须 accept/reject 的 claim，否则返回 `None`。
    ///
    /// # Errors
    ///
    /// 无错误；越界或 unknown head 返回 `None`，由 adapter 进入 terminal failure。
    pub(crate) fn claim_completion(&mut self, head: u16) -> Option<CompletionClaim> {
        self.by_head
            .get_mut(head as usize)?
            .take()
            .map(|identity| CompletionClaim { head, identity })
    }

    /// 提交已验证 completion，永久移除 descriptor mapping。
    ///
    /// # Parameters
    ///
    /// - `claim`: 当前 owner 产生且尚未消费的 claim。
    ///
    /// # Returns
    ///
    /// validated request 的完整 identity。
    ///
    /// # Errors
    ///
    /// 无错误；caller 必须先完成 adapter identity/length 验证。
    pub(crate) fn accept_completion(&mut self, claim: CompletionClaim) -> RequestIdentity {
        claim.identity
    }

    /// adapter validation 失败时把 identity 交还给 terminal failure drain。
    ///
    /// # Parameters
    ///
    /// - `claim`: 当前 owner 产生且尚未消费的 claim。
    ///
    /// # Returns
    ///
    /// 无；后续 `pop_outstanding` 将精确失败并唤醒该 request。
    ///
    /// # Errors
    ///
    /// descriptor mapping 已重新占用时 fail-stop。
    pub(crate) fn reject_completion(&mut self, claim: CompletionClaim) {
        assert!(
            self.by_head[claim.head as usize]
                .replace(claim.identity)
                .is_none(),
            "completion rejection replaced a live descriptor mapping"
        );
    }

    fn release_slot(&mut self, identity: RequestIdentity) {
        let slot = &mut self.slots[identity.slot as usize];
        assert_ne!(slot.generation, 0, "driver request slot released twice");
        assert_eq!(
            slot.generation, identity.generation,
            "stale request slot release"
        );
        self.available_descriptors += slot.descriptors as usize;
        assert!(
            self.available_descriptors <= self.by_head.len(),
            "descriptor credits released twice"
        );
        slot.generation = 0;
        slot.descriptors = 0;
        slot.next_free = self.free_slot;
        self.free_slot = Some(identity.slot);
    }

    /// 释放 caller 已消费的 slot，不执行 capacity handoff。
    ///
    /// # Parameters
    ///
    /// - `identity`: 待释放的唯一 slot/generation identity。
    ///
    /// # Errors
    ///
    /// slot 已空闲时 fail-stop。
    pub(crate) fn release_without_handoff(&mut self, identity: RequestIdentity) {
        self.release_slot(identity);
    }

    /// 释放 caller 已消费的 slot，并直接交给最旧 capacity waiter。
    ///
    /// # Parameters
    ///
    /// - `identity`: 待释放的唯一 slot/generation identity。
    ///
    /// # Returns
    ///
    /// 存在已睡眠 task waiter 时返回锁外 wake capability；否则返回 `None`。
    ///
    /// # Errors
    ///
    /// slot 重复释放或 handoff 后无法重新预留时 fail-stop。
    pub(crate) fn release_and_handoff(
        &mut self,
        identity: RequestIdentity,
    ) -> Option<CapacityWake> {
        self.release_slot(identity);
        self.handoff_next()
    }

    /// 只授予能同时取得 slot 与 descriptor credits 的 FIFO 头；不足时保留等待登记。
    /// 未进入睡眠的 waiter 不需要 wake，继续授予下一位，直到产生锁外 wake 或容量不足。
    pub(crate) fn handoff_next(&mut self) -> Option<CapacityWake> {
        loop {
            let (&ticket, waiter) = self.capacity_waiters.first_key_value()?;
            let descriptors = waiter.descriptors;
            let granted = self.reserve(descriptors)?;
            let waiter = self
                .capacity_waiters
                .remove(&ticket)
                .expect("capacity waiter disappeared");
            if let Some(wake) = waiter.publish(Ok(granted)) {
                return Some(wake);
            }
        }
    }

    /// 从 terminal failure drain 摘取最旧 capacity waiter。
    ///
    /// # Returns
    ///
    /// FIFO 为空时返回 `None`，否则返回唯一 waiter owner。
    pub(crate) fn pop_capacity_waiter(&mut self) -> Option<Arc<CapacityWait>> {
        let ticket = self
            .capacity_waiters
            .first_key_value()
            .map(|(key, _)| *key)?;
        self.capacity_waiters.remove(&ticket)
    }

    /// 撤销本 owner 尚未 handoff 的等待；已授予的结果由 caller 从 waiter 取回并归还 slot。
    /// # Panics
    /// 传入其他设备/不匹配的 FIFO owner，表示内部生命周期错误。
    pub(crate) fn cancel_capacity(
        &mut self,
        waiter: &Arc<CapacityWait>,
    ) -> Option<Arc<CapacityWait>> {
        assert_eq!(waiter.key.device, self.device);
        let crate::io_completion::IoWaitKind::Capacity(ticket) = waiter.key.kind else {
            panic!("request used as capacity wait")
        };
        if let Some(registered) = self.capacity_waiters.get(&ticket) {
            assert!(Arc::ptr_eq(registered, waiter));
        }
        self.capacity_waiters.remove(&ticket)
    }

    /// 查询 terminal failure drain 是否仍有 capacity waiter。
    ///
    /// # Returns
    ///
    /// FIFO 非空时为 true。
    pub(crate) fn has_capacity_waiters(&self) -> bool {
        !self.capacity_waiters.is_empty()
    }

    /// 从 descriptor projection 摘取一个仍 outstanding 的 request identity。
    ///
    /// # Returns
    ///
    /// 存在 live mapping 时返回 identity，否则返回 `None`。
    ///
    /// # Errors
    ///
    /// 无错误；只允许 terminal failure drain 调用。
    pub(crate) fn pop_outstanding(&mut self) -> Option<RequestIdentity> {
        self.by_head.iter_mut().find_map(Option::take)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_and_descriptor_capacity_and_identity_are_single_owned() {
        let mut owner = RequestOwner::new(8, 3, IoDevice::allocate()).unwrap();
        let ReserveOrWait::Reserved(first) = owner.reserve_or_wait(6) else {
            panic!("first slot missing")
        };
        let ReserveOrWait::Reserved(second) = owner.reserve_or_wait(2) else {
            panic!("second slot missing")
        };
        assert!(matches!(
            owner.reserve_or_wait(1),
            ReserveOrWait::Prepare(_)
        ));
        owner.publish(5, first);
        owner.publish(1, second);
        let second_claim = owner.claim_completion(1).unwrap();
        assert_eq!(owner.accept_completion(second_claim), second);
        let first_claim = owner.claim_completion(5).unwrap();
        assert_eq!(owner.accept_completion(first_claim), first);
        owner.release_without_handoff(first);
        owner.release_without_handoff(second);
        assert!(owner.slots.iter().all(|slot| slot.generation == 0));
        assert_eq!(owner.available_descriptors, 8);
    }

    #[test]
    fn rejected_completion_is_drained_and_released_exactly_once() {
        let mut owner = RequestOwner::new(8, 2, IoDevice::allocate()).unwrap();
        let ReserveOrWait::Reserved(identity) = owner.reserve_or_wait(1) else {
            panic!("slot missing")
        };
        owner.publish(3, identity);
        let claim = owner.claim_completion(3).unwrap();
        owner.reject_completion(claim);
        assert_eq!(owner.pop_outstanding(), Some(identity));
        assert_eq!(owner.pop_outstanding(), None);
        assert!(owner.claim_completion(3).is_none());
        owner.release_without_handoff(identity);
        assert!(owner.slots.iter().all(|slot| slot.generation == 0));
        assert_eq!(owner.available_descriptors, 8);
    }

    #[test]
    fn capacity_failure_and_cancellation_preserve_permits() {
        let mut owner = RequestOwner::new(8, 2, IoDevice::allocate()).unwrap();
        let ReserveOrWait::Reserved(active) = owner.reserve_or_wait(6) else {
            panic!("slot missing")
        };
        let ReserveOrWait::Prepare(ticket) = owner.reserve_or_wait(4) else {
            panic!("full owner must prepare wait")
        };
        let key = owner.capacity_key(ticket);
        let prepared = PreparedCapacityWait::try_new(
            key,
            4,
            CapacityTarget::Thread(super::super::current_wait_target()),
        )
        .unwrap();
        let CommitOrWait::Waiting(waiter) = owner.commit_wait_or_reserve(prepared) else {
            panic!("capacity unexpectedly appeared")
        };
        assert!(
            owner.handoff_next().is_none(),
            "free slot cannot replace missing descriptor credits"
        );
        let failed = owner.pop_capacity_waiter().unwrap();
        assert!(
            failed
                .publish(Err(RequestOwnerError::DeviceFailed))
                .is_none()
        );
        waiter.wait();
        assert_eq!(waiter.take_outcome(), Err(RequestOwnerError::DeviceFailed));
        owner.release_without_handoff(active);
        assert!(owner.slots.iter().all(|slot| slot.generation == 0));
        assert_eq!(owner.available_descriptors, 8);

        // 取消队首后，后续登记必须仍能取得完整 grant；已授予但未发布的 permit 也能归还。
        let ReserveOrWait::Reserved(active) = owner.reserve_or_wait(8) else {
            panic!("released capacity lost")
        };
        let enqueue = |owner: &mut RequestOwner| {
            let ReserveOrWait::Prepare(ticket) = owner.reserve_or_wait(4) else {
                panic!("capacity should be full")
            };
            let prepared = PreparedCapacityWait::try_new(
                owner.capacity_key(ticket),
                4,
                CapacityTarget::Thread(super::super::current_wait_target()),
            )
            .unwrap();
            let CommitOrWait::Waiting(waiter) = owner.commit_wait_or_reserve(prepared) else {
                panic!("capacity should remain full")
            };
            waiter
        };
        let head = enqueue(&mut owner);
        let next = enqueue(&mut owner);
        assert!(Arc::ptr_eq(&owner.cancel_capacity(&head).unwrap(), &head));
        assert!(head.try_take_outcome().is_none());
        assert!(
            owner.release_and_handoff(active).is_none(),
            "unarmed thread receives its result without a wake"
        );
        assert!(
            owner.cancel_capacity(&next).is_none(),
            "handoff already removed FIFO membership"
        );
        let grant = next.try_take_outcome().unwrap().unwrap();
        owner.release_without_handoff(grant);
        let ReserveOrWait::Reserved(all) = owner.reserve_or_wait(8) else {
            panic!("cancelled handoff leaked capacity")
        };
        owner.release_without_handoff(all);
    }
}
