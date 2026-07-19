//! @description Fixed driver request slots、descriptor identity 与 capacity wait 的共同 owner。

use alloc::{sync::Arc, vec::Vec};
use spin::Mutex;

use crate::{
    drivers::io_completion::{IoCompletion, IoDevice, IoWaitKey, IoWaitTarget},
    fallible_tree::{FallibleMap, VacantEntry},
};

/// @description request slot acquisition 的 adapter-independent failure。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::drivers) enum RequestOwnerError {
    /// waiter metadata allocation failed。
    OutOfMemory,
    /// device 已进入 terminal failure。
    DeviceFailed,
}

/// @description descriptor head 映射到的固定 slot generation identity。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::drivers) struct RequestIdentity {
    pub(in crate::drivers) slot: u16,
    pub(in crate::drivers) generation: u64,
}

/// @description 从 descriptor index 暂时摘取、尚待 adapter 验证的 completion identity。
///
/// Token 必须 exactly once 传给 `RequestOwner::accept_completion` 或
/// `RequestOwner::reject_completion`；静默丢弃会让 request 脱离 failure drain 并永久丢 wake。
#[must_use = "completion claim must be accepted or rejected exactly once"]
pub(in crate::drivers) struct CompletionClaim {
    head: u16,
    identity: RequestIdentity,
}

impl CompletionClaim {
    /// @description 在 adapter validation 期间读取 claimed request identity。
    /// @return descriptor 原先映射的完整 slot/generation identity。
    pub(in crate::drivers) fn identity(&self) -> RequestIdentity {
        self.identity
    }
}

pub(in crate::drivers) struct CapacityWait {
    key: IoWaitKey,
    completion: IoCompletion,
    target: Option<Arc<dyn IoWaitTarget>>,
    // OWNER: wait node 独占单次 slot handoff/terminal failure result。缺失该 result 会使
    // release/reset race 唤醒一个没有 permit 的 caller。
    outcome: Mutex<Option<Result<RequestIdentity, RequestOwnerError>>>,
}

impl CapacityWait {
    fn new(key: IoWaitKey, target: Option<Arc<dyn IoWaitTarget>>) -> Self {
        let completion = IoCompletion::new();
        completion.reset();
        Self {
            key,
            completion,
            target,
            outcome: Mutex::new(None),
        }
    }

    /// @description 等待 slot handoff；task 通过 scheduler sleep，bootstrap 由 adapter 提供 WFI。
    pub(in crate::drivers) fn wait(&self, mut bootstrap_wait: impl FnMut()) {
        if let Some(target) = self.target.clone() {
            target.sleep(&self.completion, self.key);
        } else {
            while !self.completion.is_complete() {
                bootstrap_wait();
            }
        }
    }

    pub(in crate::drivers) fn take_outcome(&self) -> Result<RequestIdentity, RequestOwnerError> {
        self.outcome
            .lock()
            .take()
            .expect("driver capacity waiter woke without outcome")
    }

    pub(in crate::drivers) fn publish(
        &self,
        outcome: Result<RequestIdentity, RequestOwnerError>,
    ) -> Option<CapacityWake> {
        assert!(self.outcome.lock().replace(outcome).is_none());
        if self.completion.complete() {
            self.target.clone().map(|target| CapacityWake {
                target,
                request: self.key,
            })
        } else {
            None
        }
    }
}

pub(in crate::drivers) struct CapacityWake {
    target: Arc<dyn IoWaitTarget>,
    request: IoWaitKey,
}

impl CapacityWake {
    pub(in crate::drivers) fn wake(self) {
        self.target.wake(self.request);
    }
}

pub(in crate::drivers) struct PreparedCapacityWait {
    waiter: Arc<CapacityWait>,
    entry: VacantEntry<u64, Arc<CapacityWait>>,
}

impl PreparedCapacityWait {
    pub(in crate::drivers) fn try_new(
        key: IoWaitKey,
        target: Option<Arc<dyn IoWaitTarget>>,
    ) -> Result<Self, RequestOwnerError> {
        let waiter = Arc::try_new(CapacityWait::new(key, target))
            .map_err(|_| RequestOwnerError::OutOfMemory)?;
        let crate::drivers::io_completion::IoWaitKind::Capacity(ticket) = key.kind else {
            panic!("request key used for capacity wait")
        };
        let entry = FallibleMap::try_prepare(ticket, waiter.clone())
            .map_err(|_| RequestOwnerError::OutOfMemory)?;
        Ok(Self { waiter, entry })
    }
}

pub(in crate::drivers) enum ReserveOrWait {
    Reserved(RequestIdentity),
    Prepare(u64),
}

pub(in crate::drivers) enum CommitOrWait {
    Reserved(RequestIdentity),
    Waiting(Arc<CapacityWait>),
}

/// @description 固定 slot lifecycle、descriptor-head projection 与 FIFO capacity membership。
pub(in crate::drivers) struct RequestOwner {
    by_head: Vec<Option<RequestIdentity>>,
    free_slots: Vec<u16>,
    next_generation: u64,
    inflight: usize,
    // OWNER: 本对象是 capacity membership 的唯一 FIFO index；release 必须先摘除再 wake。
    capacity_waiters: FallibleMap<u64, Arc<CapacityWait>>,
    next_capacity_ticket: u64,
    device: IoDevice,
}

impl RequestOwner {
    pub(in crate::drivers) fn new(
        queue_size: usize,
        slots: usize,
        device: IoDevice,
    ) -> Option<Self> {
        let mut by_head = Vec::new();
        by_head.try_reserve_exact(queue_size).ok()?;
        by_head.resize(queue_size, None);
        let mut free_slots = Vec::new();
        free_slots.try_reserve_exact(slots).ok()?;
        free_slots.extend((0..slots).map(|slot| slot as u16));
        Some(Self {
            by_head,
            free_slots,
            next_generation: 1,
            inflight: 0,
            capacity_waiters: FallibleMap::new(),
            next_capacity_ticket: 1,
            device,
        })
    }

    fn reserve(&mut self) -> Option<RequestIdentity> {
        let slot = self.free_slots.pop()?;
        let generation = self.next_generation;
        self.next_generation = generation
            .checked_add(1)
            .expect("driver request generation exhausted");
        self.inflight += 1;
        Some(RequestIdentity { slot, generation })
    }

    pub(in crate::drivers) fn reserve_or_wait(&mut self) -> ReserveOrWait {
        if let Some(identity) = self.reserve() {
            return ReserveOrWait::Reserved(identity);
        }
        let ticket = self.next_capacity_ticket;
        self.next_capacity_ticket = ticket
            .checked_add(1)
            .expect("driver capacity wait ticket exhausted");
        ReserveOrWait::Prepare(ticket)
    }

    pub(in crate::drivers) fn capacity_key(&self, ticket: u64) -> IoWaitKey {
        IoWaitKey::capacity(self.device, ticket)
    }

    pub(in crate::drivers) fn commit_wait_or_reserve(
        &mut self,
        prepared: PreparedCapacityWait,
    ) -> CommitOrWait {
        if let Some(identity) = self.reserve() {
            return CommitOrWait::Reserved(identity);
        }
        let PreparedCapacityWait { waiter, entry } = prepared;
        self.capacity_waiters.commit_vacant(entry);
        CommitOrWait::Waiting(waiter)
    }

    pub(in crate::drivers) fn publish(&mut self, head: u16, identity: RequestIdentity) {
        assert!(self.by_head[head as usize].replace(identity).is_none());
    }

    /// @description 暂时摘取 used descriptor 对应 identity，交给 adapter 验证。
    /// @param head device 返回的 descriptor head。
    /// @return live mapping 存在时返回必须 accept/reject 的 claim，否则返回 `None`。
    pub(in crate::drivers) fn claim_completion(&mut self, head: u16) -> Option<CompletionClaim> {
        self.by_head
            .get_mut(head as usize)?
            .take()
            .map(|identity| CompletionClaim { head, identity })
    }

    /// @description 提交已验证 completion，永久移除 descriptor mapping。
    /// @param claim 当前 owner 产生且尚未消费的 claim。
    /// @return validated request 的完整 identity。
    pub(in crate::drivers) fn accept_completion(
        &mut self,
        claim: CompletionClaim,
    ) -> RequestIdentity {
        claim.identity
    }

    /// @description adapter validation 失败时把 identity 交还给 terminal failure drain。
    /// @param claim 当前 owner 产生且尚未消费的 claim。
    /// @return 无；后续 `pop_outstanding` 将精确失败并唤醒该 request。
    pub(in crate::drivers) fn reject_completion(&mut self, claim: CompletionClaim) {
        assert!(
            self.by_head[claim.head as usize]
                .replace(claim.identity)
                .is_none(),
            "completion rejection replaced a live descriptor mapping"
        );
    }

    fn release_slot(&mut self, identity: RequestIdentity) {
        assert!(
            !self.free_slots.contains(&identity.slot),
            "driver request slot released twice"
        );
        self.free_slots.push(identity.slot);
        self.inflight -= 1;
    }

    pub(in crate::drivers) fn release_without_handoff(&mut self, identity: RequestIdentity) {
        self.release_slot(identity);
    }

    pub(in crate::drivers) fn release_and_handoff(
        &mut self,
        identity: RequestIdentity,
    ) -> Option<CapacityWake> {
        self.release_slot(identity);
        let ticket = self
            .capacity_waiters
            .first_key_value()
            .map(|(key, _)| *key)?;
        let waiter = self
            .capacity_waiters
            .remove(&ticket)
            .expect("capacity waiter disappeared");
        let granted = self
            .reserve()
            .expect("released slot must satisfy capacity waiter");
        waiter.publish(Ok(granted))
    }

    pub(in crate::drivers) fn pop_capacity_waiter(&mut self) -> Option<Arc<CapacityWait>> {
        let ticket = self
            .capacity_waiters
            .first_key_value()
            .map(|(key, _)| *key)?;
        self.capacity_waiters.remove(&ticket)
    }

    pub(in crate::drivers) fn has_capacity_waiters(&self) -> bool {
        !self.capacity_waiters.is_empty()
    }

    pub(in crate::drivers) fn pop_outstanding(&mut self) -> Option<RequestIdentity> {
        self.by_head.iter_mut().find_map(Option::take)
    }

    #[cfg(test)]
    fn inflight(&self) -> usize {
        self.inflight
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_capacity_and_descriptor_identity_are_single_owned() {
        let mut owner = RequestOwner::new(8, 2, IoDevice::Block).unwrap();
        let ReserveOrWait::Reserved(first) = owner.reserve_or_wait() else {
            panic!("first slot missing")
        };
        let ReserveOrWait::Reserved(second) = owner.reserve_or_wait() else {
            panic!("second slot missing")
        };
        assert!(matches!(owner.reserve_or_wait(), ReserveOrWait::Prepare(_)));
        owner.publish(5, first);
        owner.publish(1, second);
        let second_claim = owner.claim_completion(1).unwrap();
        assert_eq!(owner.accept_completion(second_claim), second);
        let first_claim = owner.claim_completion(5).unwrap();
        assert_eq!(owner.accept_completion(first_claim), first);
        owner.release_without_handoff(first);
        owner.release_without_handoff(second);
        assert_eq!(owner.inflight(), 0);
    }

    #[test]
    fn rejected_completion_is_drained_and_released_exactly_once() {
        let mut owner = RequestOwner::new(8, 2, IoDevice::Block).unwrap();
        let ReserveOrWait::Reserved(identity) = owner.reserve_or_wait() else {
            panic!("slot missing")
        };
        owner.publish(3, identity);
        let claim = owner.claim_completion(3).unwrap();
        owner.reject_completion(claim);
        assert_eq!(owner.pop_outstanding(), Some(identity));
        assert_eq!(owner.pop_outstanding(), None);
        assert!(owner.claim_completion(3).is_none());
        owner.release_without_handoff(identity);
        assert_eq!(owner.inflight(), 0);
    }

    #[test]
    fn terminal_failure_publishes_capacity_outcome_without_leaking_slot() {
        let mut owner = RequestOwner::new(8, 1, IoDevice::Entropy).unwrap();
        let ReserveOrWait::Reserved(active) = owner.reserve_or_wait() else {
            panic!("slot missing")
        };
        let ReserveOrWait::Prepare(ticket) = owner.reserve_or_wait() else {
            panic!("full owner must prepare wait")
        };
        let key = owner.capacity_key(ticket);
        let prepared = PreparedCapacityWait::try_new(key, None).unwrap();
        let CommitOrWait::Waiting(waiter) = owner.commit_wait_or_reserve(prepared) else {
            panic!("capacity unexpectedly appeared")
        };
        let failed = owner.pop_capacity_waiter().unwrap();
        assert!(
            failed
                .publish(Err(RequestOwnerError::DeviceFailed))
                .is_none()
        );
        waiter.wait(|| panic!("pre-completed bootstrap waiter must not WFI"));
        assert_eq!(waiter.take_outcome(), Err(RequestOwnerError::DeviceFailed));
        owner.release_without_handoff(active);
        assert_eq!(owner.inflight(), 0);
    }
}
