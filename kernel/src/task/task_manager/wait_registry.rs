use alloc::sync::Arc;
use core::{
    hint::spin_loop,
    sync::atomic::{AtomicU64, Ordering},
};

use super::{IndexedWaitKind, PollWaitKey};
use crate::{
    fallible_tree::VacantEntry,
    fs::AdvisoryLockKey,
    ipc::{PipeDirection, PipePollState},
    memory::FutexKey,
    sync::IrqMutex,
    task::WaitResult,
};

mod batch;
mod key;
mod preparation;
mod publication;
mod registration;
mod requeue;
mod shard;
mod task_source;

pub(in crate::task::task_manager) use batch::ClaimedBatch;
use key::{WaitIndexKey, pipe_direction};
pub(super) use registration::ClaimedWait;
use registration::{
    ARMED, ARMING, ARMING_LOCKED, CANCELLED, CLAIMED, CLAIMING, NOTIFIED_INTERRUPTED,
    NOTIFIED_TIMEOUT, NOTIFIED_WOKEN, NotifyOutcome, PREPARED, REQUEUEING, WaitRegistration,
};
use shard::{LockedWaitShards, WaitShard};

pub(super) const WAIT_SHARD_COUNT: usize = 16;

pub(super) struct PreparedIndex {
    shard: usize,
    node: VacantEntry<WaitIndexKey, Arc<WaitRegistration>>,
}

pub(super) struct PreparedWait {
    registration: Arc<WaitRegistration>,
    indexes: alloc::vec::Vec<PreparedIndex>,
}

pub(super) struct WaitTicket {
    id: u64,
}

#[must_use = "published waits must be armed or cancelled"]
pub(super) struct PublishedWait {
    registration: Option<Arc<WaitRegistration>>,
}

pub(super) struct WaitArmGuard<'a> {
    registration: Arc<WaitRegistration>,
    shards: LockedWaitShards<'a>,
    armed: bool,
}

impl WaitArmGuard<'_> {
    pub(super) fn arm(&mut self) -> u64 {
        assert!(!self.armed, "wait registration armed twice");
        self.registration.state.store(ARMED, Ordering::Release);
        self.armed = true;
        self.registration.id
    }
}

impl Drop for WaitArmGuard<'_> {
    fn drop(&mut self) {
        assert!(
            self.armed,
            "wait arm guard dropped before scheduling publication"
        );
        let _ = &self.shards;
    }
}

pub(super) enum CancelOutcome {
    Cancelled,
    Notified(WaitResult),
}

pub(super) struct SourceWake {
    pub(super) claimed: Option<ClaimedWait>,
    pub(super) group: Option<usize>,
}

/// @description 以稳定 source identity 分片的唯一 wait registration owner。
pub(super) struct WaitRegistry {
    next_id: AtomicU64,
    // OWNER: 每个 source key 只进入唯一 shard；registration keys 是跨 shard transaction
    // 的唯一反向 metadata。缺失固定升序会让 poll cancel 与 futex requeue 形成 ABBA。
    shards: [IrqMutex<WaitShard>; WAIT_SHARD_COUNT],
}

impl WaitRegistry {
    const fn new() -> Self {
        Self {
            next_id: AtomicU64::new(0),
            shards: [const { IrqMutex::new(WaitShard::new()) }; WAIT_SHARD_COUNT],
        }
    }

    pub(super) fn allocate_ticket(&self) -> WaitTicket {
        let id = self
            .next_id
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                current.checked_add(1)
            })
            .expect("indexed wait ID wrapped")
            + 1;
        WaitTicket { id }
    }

    pub(super) fn publish(&'static self, prepared: PreparedWait) -> PublishedWait {
        let registration = prepared.registration;
        let keys = registration.keys.lock();
        let mut shards = LockedWaitShards::lock(&self.shards, &keys);
        for index in prepared.indexes {
            shards
                .shard_mut(index.shard)
                .index
                .commit_vacant(index.node);
        }
        assert_eq!(
            registration.state.compare_exchange(
                PREPARED,
                ARMING,
                Ordering::Release,
                Ordering::Relaxed
            ),
            Ok(PREPARED),
            "wait registration published twice"
        );
        drop(shards);
        drop(keys);
        PublishedWait {
            registration: Some(registration),
        }
    }

    fn detach(
        &self,
        registration: &WaitRegistration,
    ) -> VacantEntry<WaitIndexKey, Arc<WaitRegistration>> {
        let keys = registration.keys.lock();
        let mut shards = LockedWaitShards::lock(&self.shards, &keys);
        let mut staging = None;
        for key in keys.iter().copied() {
            if matches!(key, WaitIndexKey::Task { .. }) {
                assert!(
                    staging.is_none(),
                    "wait registration has duplicate task index"
                );
                staging = Some(
                    shards
                        .shard_mut(key.shard())
                        .index
                        .take_entry(&key)
                        .expect("wait registration lost its task source node"),
                );
            } else {
                let removed = shards
                    .shard_mut(key.shard())
                    .index
                    .remove(&key)
                    .expect("wait registration lost an exact source node");
                assert_eq!(
                    removed.id, registration.id,
                    "wait source node changed owner"
                );
            }
        }
        staging.expect("wait registration has no task staging node")
    }

    fn claimed(
        registration: &WaitRegistration,
        staging: VacantEntry<WaitIndexKey, Arc<WaitRegistration>>,
    ) -> ClaimedWait {
        ClaimedWait {
            id: registration.id,
            task: registration.task.clone(),
            kind: *registration.kind.lock(),
            staging: Some(staging),
        }
    }

    fn notify(&self, registration: &Arc<WaitRegistration>, result: WaitResult) -> NotifyOutcome {
        let notified = WaitRegistration::notified_state(result);
        loop {
            match registration.state() {
                ARMING => {
                    if registration
                        .state
                        .compare_exchange(ARMING, notified, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                    {
                        drop(self.detach(registration));
                        return NotifyOutcome::BeforeArm;
                    }
                }
                ARMED => {
                    if registration
                        .state
                        .compare_exchange(ARMED, CLAIMING, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                    {
                        let staging = self.detach(registration);
                        let claimed = Self::claimed(registration, staging);
                        registration.state.store(CLAIMED, Ordering::Release);
                        return NotifyOutcome::Armed(claimed);
                    }
                }
                ARMING_LOCKED | REQUEUEING | CLAIMING => spin_loop(),
                NOTIFIED_WOKEN | NOTIFIED_TIMEOUT | NOTIFIED_INTERRUPTED | CANCELLED | CLAIMED => {
                    return NotifyOutcome::Stale;
                }
                state => panic!("invalid published wait state {state}"),
            }
        }
    }

    fn source_candidate(
        &self,
        lower: WaitIndexKey,
        cursor: Option<WaitIndexKey>,
    ) -> Option<(WaitIndexKey, Arc<WaitRegistration>)> {
        let shard = lower.shard();
        let entries = self.shards[shard].lock();
        match cursor {
            Some(cursor) => entries.index.successor(&cursor),
            None => entries.index.ceiling(&lower),
        }
        .map(|(key, registration)| (*key, registration.clone()))
    }

    fn console_group(registration: &WaitRegistration, ready: i16) -> Option<Option<usize>> {
        match *registration.kind.lock() {
            IndexedWaitKind::Console => Some(None),
            IndexedWaitKind::Poll => registration
                .poll_keys
                .as_ref()
                .and_then(|keys| keys.iter().find(|key| key.matches_console(ready)))
                .map(|key| key.wake_group()),
            _ => None,
        }
    }

    fn pipe_group(
        registration: &WaitRegistration,
        identity: usize,
        direction: PipeDirection,
        ready: i16,
        state: PipePollState,
    ) -> Option<Option<usize>> {
        match *registration.kind.lock() {
            IndexedWaitKind::Pipe {
                identity: candidate,
                condition,
            } if candidate == identity
                && condition.direction() == direction
                && state.satisfies(condition) =>
            {
                Some(None)
            }
            IndexedWaitKind::Poll => registration
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

    pub(super) fn wake_futex_one(&self, key: FutexKey, bitset: u32) -> Option<SourceWake> {
        let lower = WaitIndexKey::Futex { key, id: 0 };
        let mut cursor = None;
        loop {
            let (index, registration) = self.source_candidate(lower, cursor)?;
            let WaitIndexKey::Futex { key: candidate, .. } = index else {
                return None;
            };
            if candidate != key {
                return None;
            }
            cursor = Some(index);
            if !matches!(*registration.kind.lock(), IndexedWaitKind::Futex { bitset: waiter, .. } if waiter & bitset != 0)
            {
                continue;
            }
            match self.notify(&registration, WaitResult::Woken) {
                NotifyOutcome::BeforeArm => {
                    return Some(SourceWake {
                        claimed: None,
                        group: None,
                    });
                }
                NotifyOutcome::Armed(claimed) => {
                    return Some(SourceWake {
                        claimed: Some(claimed),
                        group: None,
                    });
                }
                NotifyOutcome::Stale => {}
            }
        }
    }

    pub(super) fn wake_advisory_one(&self, key: AdvisoryLockKey) -> Option<SourceWake> {
        let lower = WaitIndexKey::AdvisoryLock { key, id: 0 };
        let mut cursor = None;
        loop {
            let (index, registration) = self.source_candidate(lower, cursor)?;
            let WaitIndexKey::AdvisoryLock { key: candidate, .. } = index else {
                return None;
            };
            if candidate != key {
                return None;
            }
            cursor = Some(index);
            match self.notify(&registration, WaitResult::Woken) {
                NotifyOutcome::BeforeArm => {
                    return Some(SourceWake {
                        claimed: None,
                        group: None,
                    });
                }
                NotifyOutcome::Armed(claimed) => {
                    return Some(SourceWake {
                        claimed: Some(claimed),
                        group: None,
                    });
                }
                NotifyOutcome::Stale => {}
            }
        }
    }

    pub(super) fn wake_console_one(
        &self,
        exclusive: bool,
        ready: i16,
        excluded_groups: &[Option<usize>],
    ) -> Option<SourceWake> {
        let lower = WaitIndexKey::Console { exclusive, id: 0 };
        let mut cursor = None;
        loop {
            let (index, registration) = self.source_candidate(lower, cursor)?;
            let WaitIndexKey::Console {
                exclusive: candidate,
                ..
            } = index
            else {
                return None;
            };
            if candidate != exclusive {
                return None;
            }
            cursor = Some(index);
            let Some(group) = Self::console_group(&registration, ready) else {
                continue;
            };
            if group.is_some_and(|group| excluded_groups.contains(&Some(group))) {
                continue;
            }
            match self.notify(&registration, WaitResult::Woken) {
                NotifyOutcome::BeforeArm => {
                    return Some(SourceWake {
                        claimed: None,
                        group,
                    });
                }
                NotifyOutcome::Armed(claimed) => {
                    return Some(SourceWake {
                        claimed: Some(claimed),
                        group,
                    });
                }
                NotifyOutcome::Stale => {}
            }
        }
    }

    pub(super) fn wake_pipe_one(
        &self,
        identity: usize,
        direction: PipeDirection,
        exclusive: bool,
        ready: i16,
        state: PipePollState,
        excluded_groups: &crate::fallible_tree::FallibleMap<usize, ()>,
    ) -> Option<SourceWake> {
        let lower = WaitIndexKey::Pipe {
            identity,
            direction: pipe_direction(direction),
            exclusive,
            id: 0,
        };
        let mut cursor = None;
        loop {
            let (index, registration) = self.source_candidate(lower, cursor)?;
            let WaitIndexKey::Pipe {
                identity: candidate_identity,
                direction: candidate_direction,
                exclusive: candidate_exclusive,
                ..
            } = index
            else {
                return None;
            };
            if (candidate_identity, candidate_direction, candidate_exclusive)
                != (identity, pipe_direction(direction), exclusive)
            {
                return None;
            }
            cursor = Some(index);
            let Some(group) = Self::pipe_group(&registration, identity, direction, ready, state)
            else {
                continue;
            };
            if group.is_some_and(|group| excluded_groups.contains_key(&group)) {
                continue;
            }
            match self.notify(&registration, WaitResult::Woken) {
                NotifyOutcome::BeforeArm => {
                    return Some(SourceWake {
                        claimed: None,
                        group,
                    });
                }
                NotifyOutcome::Armed(claimed) => {
                    return Some(SourceWake {
                        claimed: Some(claimed),
                        group,
                    });
                }
                NotifyOutcome::Stale => {}
            }
        }
    }

    pub(super) fn expire_one(&self, now: u64) -> Option<SourceWake> {
        loop {
            let mut selected: Option<(WaitIndexKey, Arc<WaitRegistration>)> = None;
            for shard in &self.shards {
                let entries = shard.lock();
                let lower = WaitIndexKey::Deadline { deadline: 0, id: 0 };
                let Some((key @ WaitIndexKey::Deadline { deadline, .. }, registration)) =
                    entries.index.ceiling(&lower)
                else {
                    continue;
                };
                if *deadline > now {
                    continue;
                }
                if selected
                    .as_ref()
                    .is_none_or(|(candidate, _)| key < candidate)
                {
                    selected = Some((*key, registration.clone()));
                }
            }
            let (_, registration) = selected?;
            match self.notify(&registration, WaitResult::TimedOut) {
                NotifyOutcome::BeforeArm => {
                    return Some(SourceWake {
                        claimed: None,
                        group: None,
                    });
                }
                NotifyOutcome::Armed(claimed) => {
                    return Some(SourceWake {
                        claimed: Some(claimed),
                        group: None,
                    });
                }
                NotifyOutcome::Stale => {}
            }
        }
    }

    pub(super) fn has_expired_deadline(&self, now: u64) -> bool {
        self.shards.iter().any(|shard| {
            let entries = shard.lock();
            let lower = WaitIndexKey::Deadline { deadline: 0, id: 0 };
            entries.index.ceiling(&lower).is_some_and(
                |(key, _)| matches!(key, WaitIndexKey::Deadline { deadline, .. } if *deadline <= now),
            )
        })
    }
}

pub(in crate::task::task_manager) fn arm_current(
    task: &Arc<crate::task::TaskControlBlock>,
    prepared: Result<PreparedWait, ()>,
    recheck: impl FnOnce() -> Option<WaitResult>,
    membership: impl FnOnce(u64) -> crate::task::WaitMembership,
) -> Result<super::context_switch::PreparedBlock, WaitResult> {
    let prepared = match prepared {
        Ok(prepared) => prepared,
        Err(()) => return Err(recheck().unwrap_or(WaitResult::OutOfMemory)),
    };
    let published = WAIT_REGISTRY.publish(prepared);
    if let Some(result) = recheck() {
        return Err(match published.cancel() {
            CancelOutcome::Cancelled => result,
            CancelOutcome::Notified(result) => result,
        });
    }
    let arm = published.prepare_arm()?;
    Ok(super::context_switch::prepare_current_block(
        task,
        arm,
        move |arm, _| membership(arm.arm()),
    ))
}

// OWNER: source shards and each registration's exact key list jointly own all live wait
// memberships；不存在 global queue fallback 或 lazy stale source node。
pub(super) static WAIT_REGISTRY: WaitRegistry = WaitRegistry::new();
