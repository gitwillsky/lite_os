#![feature(allocator_ext)]
#![allow(dead_code)]
//! 执行实际 shm ID/key、权限、reservation/VMA nattch 和延迟销毁；物理页生产为输入 seam。
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
use ipc::{IpcAccess, IpcError, IpcNamespace};
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
    let view = namespace::initial_pid()
        .child(namespace::initial_user(), 0)
        .unwrap();
    let mut prepared =
        namespace::PreparedPid::try_new(999, view, &[], &namespace::root_authority()).unwrap();
    prepared.publish().unwrap();
    (prepared.identity(), prepared.take_registration())
}
#[test]
fn rmid_withdraws_only_key_and_allows_reattach_until_last_actual_vma() {
    let access = access();
    let creator_list = ipc::SharedMemoryCreatorList::new().unwrap();
    let owner = owner();
    let (pid, _registration) = pid();
    let id = owner
        .shared_memory_get(
            13,
            5000,
            IPC_CREAT | 0o600,
            &access,
            pid.clone(),
            4,
            &creator_list,
        )
        .unwrap();
    let reservation = owner.shared_memory_prepare_attach(id, 6, &access).unwrap();
    let segment = reservation.segment.clone();
    let backing = Arc::downgrade(&segment.backing());
    owner.shared_memory_open(&segment, pid.clone(), 5);
    drop(reservation);
    assert_eq!(
        owner
            .shared_memory_snapshot(id, false, false, &access)
            .unwrap()
            .attaches,
        1
    );
    owner.shared_memory_remove(id, &access).unwrap();
    let old = owner
        .shared_memory_snapshot(id, false, false, &access)
        .unwrap();
    assert_eq!(old.permission.key, 0);
    assert_ne!(old.permission.mode & SHM_DEST, 0);
    let replacement = owner
        .shared_memory_get(
            13,
            4096,
            IPC_CREAT | IPC_EXCL | 0o600,
            &access,
            pid.clone(),
            6,
            &creator_list,
        )
        .unwrap();
    assert_ne!(replacement, id);
    let later = owner.shared_memory_prepare_attach(id, 4, &access).unwrap();
    owner.shared_memory_open(&segment, pid.clone(), 7);
    drop(later);
    owner.shared_memory_close(&segment, Some((pid.clone(), 8)));
    assert_eq!(
        owner
            .shared_memory_snapshot(id, false, false, &access)
            .unwrap()
            .attaches,
        1
    );
    assert!(backing.upgrade().is_some());
    owner.shared_memory_close(&segment, Some((pid.clone(), 9)));
    assert!(
        backing.upgrade().is_none(),
        "segment observer must not retain actual backing"
    );
    assert!(matches!(
        owner.shared_memory_snapshot(id, false, false, &access),
        Err(IpcError::Invalid)
    ));
    assert!(matches!(
        segment.snapshot(&access, false),
        Err(IpcError::Removed)
    ));
    assert_eq!(
        owner
            .shared_memory_get(13, 0, 0, &access, pid.clone(), 10, &creator_list)
            .unwrap(),
        replacement
    );
    owner.shared_memory_remove(replacement, &access).unwrap();
}
#[test]
fn failed_mapping_reservation_does_not_update_vma_times_or_pid() {
    let access = access();
    let creator_list = ipc::SharedMemoryCreatorList::new().unwrap();
    let owner = owner();
    let (pid, _registration) = pid();
    let id = owner
        .shared_memory_get(0, 1, 0o600, &access, pid.clone(), 21, &creator_list)
        .unwrap();
    let reservation = owner.shared_memory_prepare_attach(id, 6, &access).unwrap();
    let segment = reservation.segment.clone();
    assert_eq!(
        owner
            .shared_memory_snapshot(id, false, false, &access)
            .unwrap()
            .attaches,
        1
    );
    drop(reservation);
    let snapshot = segment.snapshot(&access, false).unwrap();
    assert_eq!(
        (
            snapshot.attaches,
            snapshot.attach_time,
            snapshot.detach_time,
            snapshot.change_time
        ),
        (0, 0, 0, 21)
    );
    assert!(snapshot.last_operator.is_none());
    let reservation = owner.shared_memory_prepare_attach(id, 4, &access).unwrap();
    owner.shared_memory_remove(id, &access).unwrap();
    assert!(
        owner
            .shared_memory_snapshot(id, false, false, &access)
            .is_ok()
    );
    drop(reservation);
    assert!(matches!(
        segment.snapshot(&access, false),
        Err(IpcError::Removed)
    ));
}
#[test]
fn namespace_shm_identity_size_and_vma_actor_are_actual_object_facts() {
    let access = access();
    let creator_list = ipc::SharedMemoryCreatorList::new().unwrap();
    let first = owner();
    let second = IpcNamespace::create(namespace::boot(), 0).unwrap();
    let (creator, _creator_registration) = pid();
    let (actor, _actor_registration) = pid();
    let first_id = first
        .shared_memory_get(
            8,
            8192,
            IPC_CREAT | 0o600,
            &access,
            creator.clone(),
            30,
            &creator_list,
        )
        .unwrap();
    let second_id = second
        .shared_memory_get(
            8,
            32,
            IPC_CREAT | 0o600,
            &access,
            actor.clone(),
            31,
            &creator_list,
        )
        .unwrap();
    assert_eq!(first_id, second_id);
    assert!(matches!(
        first.shared_memory_get(8, 9000, 0, &access, actor.clone(), 32, &creator_list),
        Err(IpcError::Invalid)
    ));
    assert!(matches!(
        first.shared_memory_get(
            8,
            1,
            IPC_CREAT | IPC_EXCL,
            &access,
            actor.clone(),
            32,
            &creator_list
        ),
        Err(IpcError::Exists)
    ));
    let reservation = first
        .shared_memory_prepare_attach(first_id, 6, &access)
        .unwrap();
    let segment = reservation.segment.clone();
    first.shared_memory_open(&segment, actor.clone(), 33);
    drop(reservation);
    // 真实 fork/split 新增 VMA；Arc clone 不改变计数。
    let observer = segment.clone();
    first.shared_memory_open(&segment, creator.clone(), 34);
    let snapshot = first
        .shared_memory_snapshot(first_id, false, false, &access)
        .unwrap();
    assert_eq!(snapshot.attaches, 2);
    assert!(Arc::ptr_eq(&snapshot.last_operator.unwrap(), &creator));
    first.shared_memory_close(&segment, Some((actor.clone(), 35)));
    first.shared_memory_close(&segment, Some((creator.clone(), 36)));
    let snapshot = observer.snapshot(&access, false).unwrap();
    assert_eq!((snapshot.attaches, snapshot.detach_time), (0, 36));
    first.shared_memory_remove(first_id, &access).unwrap();
    second.shared_memory_remove(second_id, &access).unwrap();
}
#[test]
fn namespace_page_admission_charges_until_actual_delayed_destroy_and_limits_recheck() {
    let access = access();
    let creator_list = ipc::SharedMemoryCreatorList::new().unwrap();
    let owner = owner();
    let (pid, _registration) = pid();
    owner
        .set_shared_memory_limit(ipc::SharedMemoryLimit::TotalPages, 2, &access.authority)
        .unwrap();
    let id = owner
        .shared_memory_get(0, 4097, 0o600, &access, pid.clone(), 40, &creator_list)
        .unwrap();
    let reservation = owner.shared_memory_prepare_attach(id, 4, &access).unwrap();
    owner.shared_memory_remove(id, &access).unwrap();
    assert!(matches!(
        owner.shared_memory_get(0, 1, 0o600, &access, pid.clone(), 41, &creator_list),
        Err(IpcError::NoSpace)
    ));
    assert_eq!(owner.shared_memory_info().1, [2, 0, 0]);
    drop(reservation);
    let next = owner
        .shared_memory_get(0, 1, 0o600, &access, pid.clone(), 42, &creator_list)
        .unwrap();
    owner
        .set_shared_memory_limit(ipc::SharedMemoryLimit::Segments, 0, &access.authority)
        .unwrap();
    assert!(matches!(
        owner.shared_memory_get(0, 1, 0o600, &access, pid.clone(), 43, &creator_list),
        Err(IpcError::NoSpace)
    ));
    owner.shared_memory_remove(next, &access).unwrap();
    assert_eq!(owner.shared_memory_info().0, 0);
}
#[test]
fn forced_removal_uses_real_creator_retirement_instead_of_pid_observer_lifetime() {
    let access = access();
    let owner = owner();
    let creator = ipc::SharedMemoryCreatorList::new().unwrap();
    let creator_observer = creator.clone();
    let (pid, _registration) = pid();
    let unused = owner
        .shared_memory_get(91, 1, IPC_CREAT | 0o600, &access, pid.clone(), 50, &creator)
        .unwrap();
    let attached = owner
        .shared_memory_get(92, 1, IPC_CREAT | 0o600, &access, pid.clone(), 51, &creator)
        .unwrap();
    let reservation = owner
        .shared_memory_prepare_attach(attached, 4, &access)
        .unwrap();
    let segment = reservation.segment.clone();
    owner.shared_memory_open(&segment, pid.clone(), 52);
    drop(reservation);
    owner
        .set_shared_memory_limit(ipc::SharedMemoryLimit::ForcedRemove, 1, &access.authority)
        .unwrap();
    assert!(
        owner
            .shared_memory_snapshot(unused, false, false, &access)
            .is_ok(),
        "live creator retains never-attached segment"
    );
    creator.retire();
    assert!(matches!(
        owner.shared_memory_snapshot(unused, false, false, &access),
        Err(IpcError::Invalid)
    ));
    assert_eq!(
        owner
            .shared_memory_snapshot(attached, false, false, &access)
            .unwrap()
            .attaches,
        1
    );
    owner.shared_memory_close(&segment, Some((pid.clone(), 53)));
    assert!(matches!(
        owner.shared_memory_snapshot(attached, false, false, &access),
        Err(IpcError::Invalid)
    ));
    assert!(
        Arc::strong_count(&creator_observer) >= 2,
        "observer does not restore creator phase"
    );
}
#[test]
fn enabling_forced_removal_drains_previously_orphaned_segments_and_namespace_drop_unlinks_creator()
{
    let access = access();
    let owner = owner();
    let creator = ipc::SharedMemoryCreatorList::new().unwrap();
    let (pid, _registration) = pid();
    let id = owner
        .shared_memory_get(0, 1, 0o600, &access, pid.clone(), 60, &creator)
        .unwrap();
    creator.retire();
    assert!(
        owner
            .shared_memory_snapshot(id, false, false, &access)
            .is_ok()
    );
    owner
        .set_shared_memory_limit(ipc::SharedMemoryLimit::ForcedRemove, 1, &access.authority)
        .unwrap();
    assert!(matches!(
        owner.shared_memory_snapshot(id, false, false, &access),
        Err(IpcError::Invalid)
    ));
    let second = IpcNamespace::create(namespace::boot(), 0).unwrap();
    let live_creator = ipc::SharedMemoryCreatorList::new().unwrap();
    second
        .shared_memory_get(0, 1, 0o600, &access, pid, 61, &live_creator)
        .unwrap();
    drop(second);
    // Namespace teardown 必须撤销 clist 节点；没有段时 live creator 的冷 Drop 可直接归还 Arc。
    drop(live_creator);
}
#[test]
fn shm_lock_charges_real_uid_once_and_unlock_and_destroy_return_original_charge() {
    let mut access = access();
    access.authority.effective_capabilities = 0;
    let owner = owner();
    let creator = ipc::SharedMemoryCreatorList::new().unwrap();
    let (pid, _registration) = pid();
    let first = owner
        .shared_memory_get(0, 8192, 0o600, &access, pid.clone(), 70, &creator)
        .unwrap();
    let second = owner
        .shared_memory_get(0, 4096, 0o600, &access, pid.clone(), 71, &creator)
        .unwrap();
    owner
        .shared_memory_lock(first, true, &access, 591201, || 8192)
        .unwrap();
    owner
        .shared_memory_lock(first, true, &access, 591201, || 8192)
        .unwrap();
    assert_ne!(
        owner
            .shared_memory_snapshot(first, false, false, &access)
            .unwrap()
            .permission
            .mode
            & SHM_LOCKED,
        0
    );
    assert!(matches!(
        owner.shared_memory_lock(second, true, &access, 591201, || 8192),
        Err(IpcError::NoMemory)
    ));
    assert!(matches!(
        owner.shared_memory_lock(first, true, &access, 591201, || 0),
        Err(IpcError::Permission)
    ));
    owner
        .shared_memory_lock(first, false, &access, 591202, || 0)
        .unwrap();
    owner
        .shared_memory_lock(second, true, &access, 591201, || 8192)
        .unwrap();
    owner.shared_memory_remove(second, &access).unwrap();
    owner
        .shared_memory_lock(first, true, &access, 591201, || 8192)
        .unwrap();
    owner.shared_memory_remove(first, &access).unwrap();
}
#[test]
fn shm_lock_reads_current_rlimit_after_policy_wait_and_rolls_back_failed_admission() {
    let mut access = access();
    access.authority.effective_capabilities = 0;
    let owner = owner();
    let creator = ipc::SharedMemoryCreatorList::new().unwrap();
    let (pid, _registration) = pid();
    let id = owner
        .shared_memory_get(0, 1, 0o600, &access, pid, 80, &creator)
        .unwrap();
    let reads = core::cell::Cell::new(0);
    let result = owner.shared_memory_lock(id, true, &access, 591203, || {
        let previous = reads.replace(reads.get() + 1);
        if previous == 0 { 4096 } else { 0 }
    });
    assert!(matches!(result, Err(IpcError::Permission)));
    assert_eq!(reads.get(), 2);
    assert_eq!(
        owner
            .shared_memory_snapshot(id, false, false, &access)
            .unwrap()
            .permission
            .mode
            & SHM_LOCKED,
        0
    );
    owner
        .shared_memory_lock(id, true, &access, 591203, || 4096)
        .unwrap();
    owner.shared_memory_remove(id, &access).unwrap();
}

#[path = "."]
pub(crate) mod execution {
    #[path = "../src/sync.rs"]
    pub(crate) mod sync;
}
