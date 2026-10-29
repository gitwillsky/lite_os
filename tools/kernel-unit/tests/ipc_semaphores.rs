#![feature(allocator_ext)]
#![allow(dead_code)]
//! 实际 IPC IDs、semaphore array、atomic admission 与 SEM_UNDO owner；未复制操作算法。
extern crate alloc;
#[path = "../../../kernel/src/fallible_tree/mod.rs"]
mod fallible_tree;
#[path = "../src/ipc_namespace_fixture.rs"]
mod ipc;
#[path = "../src/shm_backing_fixture.rs"]
mod memory;
#[path = "../src/namespace_fixture.rs"]
mod namespace;

use alloc::sync::Arc;
use ipc::{
    IpcAccess, IpcError, IpcNamespace, SemaphoreChange, SemaphoreOperation, SemaphoreSet,
    SemaphoreUndoHandle,
};
use syscall_abi::ipc::*;

fn access() -> IpcAccess {
    namespace::boot();
    IpcAccess {
        authority: namespace::root_authority(),
        filesystem_gid: 0,
        groups: None,
    }
}
fn owner() -> Arc<IpcNamespace> {
    IpcNamespace::create(namespace::boot(), 0).unwrap()
}
fn pid() -> (Arc<namespace::PidIdentity>, namespace::PidRegistration) {
    let authority = namespace::root_authority();
    let view = namespace::initial_pid()
        .child(namespace::initial_user(), 0)
        .unwrap();
    let mut prepared = namespace::PreparedPid::try_new(999, view, &[], &authority).unwrap();
    prepared.publish().unwrap();
    (prepared.identity(), prepared.take_registration())
}
fn change(index: u16, delta: i16, flags: u16) -> SemaphoreChange {
    SemaphoreChange {
        index,
        delta,
        flags,
    }
}
fn operation(
    set: &Arc<SemaphoreSet>,
    changes: &[SemaphoreChange],
    handle: Option<&SemaphoreUndoHandle>,
    pid: &Arc<namespace::PidIdentity>,
) -> SemaphoreOperation {
    let undo = handle.map(|handle| handle.list().entry(set).unwrap());
    SemaphoreOperation::new(set, changes.to_vec(), undo, pid.clone()).unwrap()
}
#[test]
fn ipc_types_have_independent_ids_keys_and_atomic_admission_usage() {
    let first = owner();
    let second = owner();
    let access = access();
    assert_eq!(
        first.semaphore_get(0, 0, 0o600, &access, 1),
        Err(IpcError::Invalid)
    );
    let id = first
        .semaphore_get(77, 2, IPC_CREAT | 0o600, &access, 1)
        .unwrap();
    first
        .message_get(77, IPC_CREAT | 0o600, &access, 1)
        .unwrap();
    assert_eq!(first.semaphore_get(77, 0, 0, &access, 1), Ok(id));
    assert_eq!(
        first.semaphore_get(77, 3, 0, &access, 1),
        Err(IpcError::Invalid)
    );
    assert_eq!(
        first.semaphore_get(77, 2, IPC_CREAT | IPC_EXCL, &access, 1),
        Err(IpcError::Exists)
    );
    assert_eq!(
        second.semaphore_get(77, 0, 0, &access, 1),
        Err(IpcError::NotFound)
    );
    let (info, _) = first.semaphore_info(true);
    assert_eq!((info[7], info[9]), (1, 2));
    first.semaphore_remove(id, &access).unwrap();
    assert_eq!(first.semaphore_info(true).0[9], 0);
    assert!(matches!(first.semaphore_set(id), Err(IpcError::Invalid)));
}
#[test]
fn atomic_duplicate_operations_rollback_without_pid_or_time_side_effects() {
    let owner = owner();
    let access = access();
    let (pid, _registration) = pid();
    let id = owner.semaphore_get(0, 2, 0o600, &access, 1).unwrap();
    let set = owner.semaphore_set(id).unwrap();
    set.set_values(None, &[1, 0], &access, &pid, 2).unwrap();
    let mut attempt = operation(
        &set,
        &[change(0, -1, 0), change(1, -1, IPC_NOWAIT as u16)],
        None,
        &pid,
    );
    assert_eq!(set.operate(&mut attempt, &access, 3), Err(IpcError::Again));
    assert_eq!(set.values(&access).unwrap(), [1, 0]);
    assert_eq!(set.snapshot(&access, false).unwrap().operation_time, 0);
    let mut duplicate = operation(
        &set,
        &[change(0, 1, 0), change(0, -2, 0), change(1, 0, 0)],
        None,
        &pid,
    );
    assert!(set.operate(&mut duplicate, &access, 4).unwrap());
    assert_eq!(set.values(&access).unwrap(), [0, 0]);
    let mut overflow = operation(&set, &[change(0, 32767, 0), change(0, 1, 0)], None, &pid);
    assert_eq!(set.operate(&mut overflow, &access, 5), Err(IpcError::Range));
    assert_eq!(set.values(&access).unwrap(), [0, 0]);
    assert_eq!(set.snapshot(&access, false).unwrap().operation_time, 4);
}
#[test]
fn blocking_counters_follow_first_unsatisfied_operation_and_complete_atomically() {
    let owner = owner();
    let access = access();
    let (pid, _registration) = pid();
    let id = owner.semaphore_get(0, 2, 0o600, &access, 1).unwrap();
    let set = owner.semaphore_set(id).unwrap();
    let mut waiter = operation(&set, &[change(0, -1, 0), change(1, -1, 0)], None, &pid);
    assert!(!set.operate(&mut waiter, &access, 2).unwrap());
    assert_eq!(
        set.scalar(0, GETNCNT, &access, &pid.namespace()).unwrap(),
        1
    );
    set.set_values(None, &[1, 0], &access, &pid, 3).unwrap();
    assert_eq!(set.values(&access).unwrap(), [1, 0]);
    assert_eq!(
        set.scalar(0, GETNCNT, &access, &pid.namespace()).unwrap(),
        0
    );
    assert_eq!(
        set.scalar(1, GETNCNT, &access, &pid.namespace()).unwrap(),
        1
    );
    set.set_values(Some(1), &[1], &access, &pid, 4).unwrap();
    assert_eq!(waiter.wait(), crate::execution::sync::WaitResult::Woken);
    assert_eq!(waiter.finish_wait(), Some(Ok(())));
    assert_eq!(set.values(&access).unwrap(), [0, 0]);
    assert_eq!(
        set.scalar(1, GETNCNT, &access, &pid.namespace()).unwrap(),
        0
    );
}
#[test]
fn shared_undo_applies_only_at_last_participant_and_setval_clears_adjustments() {
    let owner = owner();
    let access = access();
    let (pid, _registration) = pid();
    let id = owner.semaphore_get(0, 1, 0o600, &access, 1).unwrap();
    let set = owner.semaphore_set(id).unwrap();
    set.set_values(None, &[5], &access, &pid, 2).unwrap();
    let mut first = SemaphoreUndoHandle::new().unwrap();
    let mut shared = first.fork(true).unwrap();
    let mut fresh = first.fork(false).unwrap();
    let mut decrement = operation(&set, &[change(0, -2, SEM_UNDO)], Some(&first), &pid);
    assert!(set.operate(&mut decrement, &access, 3).unwrap());
    shared.retire(&pid, 4);
    fresh.retire(&pid, 5);
    assert_eq!(set.values(&access).unwrap(), [3]);
    first.retire(&pid, 6);
    assert_eq!(set.values(&access).unwrap(), [5]);
    assert_eq!(set.snapshot(&access, false).unwrap().operation_time, 6);
    let mut reset = SemaphoreUndoHandle::new().unwrap();
    let mut decrement = operation(&set, &[change(0, -1, SEM_UNDO)], Some(&reset), &pid);
    assert!(set.operate(&mut decrement, &access, 7).unwrap());
    set.set_values(Some(0), &[9], &access, &pid, 8).unwrap();
    reset.retire(&pid, 9);
    assert_eq!(set.values(&access).unwrap(), [9]);
}
#[test]
fn rmid_removes_both_undo_indices_and_wakes_exact_pending_operation() {
    let owner = owner();
    let access = access();
    let (pid, _registration) = pid();
    let id = owner.semaphore_get(0, 1, 0o600, &access, 1).unwrap();
    let set = owner.semaphore_set(id).unwrap();
    let handle = SemaphoreUndoHandle::new().unwrap();
    let mut wait = operation(&set, &[change(0, -1, SEM_UNDO)], Some(&handle), &pid);
    assert!(!set.operate(&mut wait, &access, 2).unwrap());
    owner.semaphore_remove(id, &access).unwrap();
    assert_eq!(wait.wait(), crate::execution::sync::WaitResult::Woken);
    assert_eq!(wait.finish_wait(), Some(Err(IpcError::Removed)));
    assert!(matches!(
        set.snapshot(&access, false),
        Err(IpcError::Removed)
    ));
    drop(handle); // actual list 已被 RMID 摘空，无遗留调整或失效 Vec。
    let next = owner.semaphore_get(0, 1, 0o600, &access, 3).unwrap();
    assert_ne!(next, id);
}
#[test]
fn final_namespace_drop_retires_arrays_even_when_observers_still_exist() {
    let owner = owner();
    let access = access();
    let (pid, _registration) = pid();
    let id = owner.semaphore_get(0, 1, 0o600, &access, 1).unwrap();
    let set = owner.semaphore_set(id).unwrap();
    let handle = SemaphoreUndoHandle::new().unwrap();
    let _entry = handle.list().entry(&set).unwrap();
    drop(owner);
    assert!(matches!(set.values(&access), Err(IpcError::Removed)));
    drop(handle);
    drop(pid);
}

#[path = "../src/interruptible_event_target.rs"]
mod timed_target;
#[test]
fn expired_semaphore_wait_cancels_actual_admission_without_consuming_future_values() {
    let owner = owner();
    let access = access();
    let (pid, _registration) = pid();
    let id = owner.semaphore_get(0, 1, 0o600, &access, 1).unwrap();
    let set = owner.semaphore_set(id).unwrap();
    let mut wait = operation(&set, &[change(0, -1, 0)], None, &pid);
    assert!(!set.operate(&mut wait, &access, 2).unwrap());
    let target = timed_target::Target::new();
    let _binding = target.bind();
    assert_eq!(
        wait.wait_until(Some(0)),
        crate::execution::sync::WaitResult::TimedOut
    );
    assert!(wait.finish_wait().is_none());
    assert_eq!(
        set.scalar(0, GETNCNT, &access, &pid.namespace()).unwrap(),
        0
    );
    set.set_values(None, &[2], &access, &pid, 3).unwrap();
    assert_eq!(set.values(&access).unwrap(), [2]);
    assert!(wait.finish_wait().is_none());
}
#[test]
fn semaphore_limits_charge_registry_and_keep_signed_dointvec_semantics() {
    let owner = owner();
    let access = access();
    owner
        .set_semaphore_limits(&[Some(2), Some(3), Some(1), Some(2)], &access.authority)
        .unwrap();
    let first = owner.semaphore_get(0, 2, 0o600, &access, 1).unwrap();
    assert_eq!(
        owner.semaphore_get(0, 2, 0o600, &access, 1),
        Err(IpcError::NoSpace)
    );
    let second = owner.semaphore_get(0, 1, 0o600, &access, 1).unwrap();
    assert_eq!(
        owner.semaphore_get(0, 1, 0o600, &access, 1),
        Err(IpcError::NoSpace)
    );
    owner.semaphore_remove(first, &access).unwrap();
    owner.semaphore_remove(second, &access).unwrap();
    assert_eq!(
        owner.set_semaphore_limits(&[Some(-1), Some(-2), Some(-3), Some(-1)], &access.authority),
        Err(IpcError::Range)
    );
    assert_eq!(owner.semaphore_limits(), [-1, -2, -3, 2]);
    assert_eq!(
        owner.semaphore_get(0, 1, 0o600, &access, 1),
        Err(IpcError::Invalid)
    );
}

#[path = "../src/allocation_failure.rs"]
mod allocation_failure;
#[test]
fn creation_and_undo_preparation_failures_leave_no_ids_quota_or_cross_registration() {
    let owner = owner();
    let access = access();
    for index in 0..4 {
        allocation_failure::fail_after(index);
        let result = owner.semaphore_get(101, 2, IPC_CREAT | 0o600, &access, 1);
        let consumed = allocation_failure::was_consumed();
        assert_eq!(result, Err(IpcError::NoMemory));
        assert!(consumed);
        assert_eq!(owner.semaphore_info(true).0[9], 0);
        assert_eq!(
            owner.semaphore_get(101, 0, 0, &access, 1),
            Err(IpcError::NotFound)
        );
    }
    let id = owner
        .semaphore_get(101, 2, IPC_CREAT | 0o600, &access, 1)
        .unwrap();
    assert_eq!(id, 0);
    let set = owner.semaphore_set(id).unwrap();
    for index in 0..5 {
        let handle = SemaphoreUndoHandle::new().unwrap();
        allocation_failure::fail_after(index);
        let result = handle.list().entry(&set);
        let consumed = allocation_failure::was_consumed();
        assert!(matches!(result, Err(IpcError::NoMemory)));
        assert!(consumed);
        drop(handle); // 最后 membership 回滚必须仍为空；不能留下只发布一侧的 entry。
        assert_eq!(set.values(&access).unwrap(), [0, 0]);
    }
    owner.semaphore_remove(id, &access).unwrap();
}
#[test]
fn pending_operation_keeps_admitted_permissions_while_new_operations_use_updated_mode() {
    let owner = owner();
    let admin = access();
    let mut reader = access();
    reader.authority.effective_capabilities = 0;
    let (pid, _registration) = pid();
    let id = owner.semaphore_get(0, 1, 0o600, &admin, 1).unwrap();
    let set = owner.semaphore_set(id).unwrap();
    let mut pending = operation(&set, &[change(0, -1, 0)], None, &pid);
    assert!(!set.operate(&mut pending, &reader, 2).unwrap());
    owner.semaphore_update(id, 0, 0, 0, &admin, 3).unwrap();
    let mut later = operation(&set, &[change(0, -1, 0)], None, &pid);
    assert_eq!(set.operate(&mut later, &reader, 4), Err(IpcError::Access));
    set.set_values(None, &[1], &admin, &pid, 5).unwrap();
    assert_eq!(pending.wait(), crate::execution::sync::WaitResult::Woken);
    assert_eq!(pending.finish_wait(), Some(Ok(())));
    assert_eq!(set.values(&admin).unwrap(), [0]);
}
#[test]
fn undo_negative_bound_includes_int16_minimum_and_overflow_rolls_back_prior_changes() {
    let owner = owner();
    let access = access();
    let (pid, _registration) = pid();
    let id = owner.semaphore_get(0, 2, 0o600, &access, 1).unwrap();
    let set = owner.semaphore_set(id).unwrap();
    let mut handle = SemaphoreUndoHandle::new().unwrap();
    for (delta, flags) in [(32767, SEM_UNDO), (-32767, 0), (1, SEM_UNDO)] {
        let mut op = operation(&set, &[change(0, delta, flags)], Some(&handle), &pid);
        assert!(set.operate(&mut op, &access, 2).unwrap());
    }
    let mut overflow = operation(
        &set,
        &[change(1, 5, SEM_UNDO), change(0, 1, SEM_UNDO)],
        Some(&handle),
        &pid,
    );
    assert_eq!(set.operate(&mut overflow, &access, 3), Err(IpcError::Range));
    assert_eq!(set.values(&access).unwrap(), [1, 0]);
    handle.retire(&pid, 4);
    assert_eq!(set.values(&access).unwrap(), [0, 0]);
    assert_eq!(set.snapshot(&access, false).unwrap().operation_time, 4);
}

#[path = "."]
pub(crate) mod execution {
    #[path = "../src/sync.rs"]
    pub(crate) mod sync;
}
