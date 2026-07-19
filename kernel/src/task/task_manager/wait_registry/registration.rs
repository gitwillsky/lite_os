use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicU8, Ordering};
use spin::Mutex;

use super::{IndexedWaitKind, PollWaitKey, key::WaitIndexKey};
use crate::{
    fallible_tree::VacantEntry,
    task::{TaskControlBlock, WaitResult},
};

pub(super) const PREPARED: u8 = 0;
pub(super) const ARMING: u8 = 1;
pub(super) const ARMING_LOCKED: u8 = 2;
pub(super) const ARMED: u8 = 3;
pub(super) const REQUEUEING: u8 = 4;
pub(super) const CLAIMING: u8 = 5;
pub(super) const NOTIFIED_WOKEN: u8 = 6;
pub(super) const NOTIFIED_TIMEOUT: u8 = 7;
pub(super) const NOTIFIED_INTERRUPTED: u8 = 8;
pub(super) const CANCELLED: u8 = 9;
pub(super) const CLAIMED: u8 = 10;

pub(super) struct WaitRegistration {
    pub(super) id: u64,
    pub(super) task: Arc<TaskControlBlock>,
    pub(super) kind: Mutex<IndexedWaitKind>,
    pub(super) poll_keys: Option<Vec<PollWaitKey>>,
    pub(super) keys: Mutex<Vec<WaitIndexKey>>,
    pub(super) state: AtomicU8,
}

impl WaitRegistration {
    pub(super) fn notified_state(result: WaitResult) -> u8 {
        match result {
            WaitResult::Woken => NOTIFIED_WOKEN,
            WaitResult::TimedOut => NOTIFIED_TIMEOUT,
            WaitResult::Interrupted => NOTIFIED_INTERRUPTED,
            WaitResult::OutOfMemory => panic!("published wait cannot be notified by OOM"),
        }
    }

    pub(super) fn notified_result(state: u8) -> Option<WaitResult> {
        match state {
            NOTIFIED_WOKEN => Some(WaitResult::Woken),
            NOTIFIED_TIMEOUT => Some(WaitResult::TimedOut),
            NOTIFIED_INTERRUPTED => Some(WaitResult::Interrupted),
            _ => None,
        }
    }

    pub(super) fn state(&self) -> u8 {
        self.state.load(Ordering::Acquire)
    }
}

pub(in crate::task::task_manager) struct ClaimedWait {
    pub(in crate::task::task_manager) id: u64,
    pub(in crate::task::task_manager) task: Arc<TaskControlBlock>,
    pub(in crate::task::task_manager) kind: IndexedWaitKind,
    pub(super) staging: Option<VacantEntry<WaitIndexKey, Arc<WaitRegistration>>>,
}

pub(super) enum NotifyOutcome {
    BeforeArm,
    Armed(ClaimedWait),
    Stale,
}
