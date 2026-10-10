#![feature(allocator_ext)]
#![allow(dead_code)]
//! 生产 UserNS/kuid locked-page ucounts、祖先限额与收费 token；不模拟配额算法。
extern crate alloc;
#[path = "../../../kernel/src/fallible_tree/mod.rs"]
mod fallible_tree;
#[path = "../src/namespace_fixture.rs"]
mod namespace;
use namespace::{NamespaceError, RlimitCharge, RlimitResource, UserNamespace};

#[test]
fn locked_memory_sums_real_uid_across_users_and_charge_drop_returns_original_owner() {
    let user = namespace::boot();
    let first = RlimitCharge::acquire(
        user.clone(),
        591101,
        RlimitResource::LockedMemory,
        2,
        3,
        false,
    )
    .unwrap();
    assert!(matches!(
        RlimitCharge::acquire(
            user.clone(),
            591101,
            RlimitResource::LockedMemory,
            2,
            3,
            false
        ),
        Err(NamespaceError::QuotaLimit)
    ));
    let other = RlimitCharge::acquire(
        user.clone(),
        591102,
        RlimitResource::LockedMemory,
        3,
        3,
        false,
    )
    .unwrap();
    drop(first);
    let replacement =
        RlimitCharge::acquire(user, 591101, RlimitResource::LockedMemory, 3, 3, false).unwrap();
    drop((replacement, other));
}
#[test]
fn child_lock_capability_does_not_bypass_inherited_ancestor_ucounts() {
    namespace::boot();
    let mut authority = namespace::root_authority();
    authority.effective_uid = 591103;
    let child = UserNamespace::child(
        &authority,
        true,
        namespace::NamespaceRlimits {
            locked_memory: 1,
            pending_signals: u64::MAX,
        },
    )
    .unwrap();
    let first = RlimitCharge::acquire(
        child.clone(),
        0,
        RlimitResource::LockedMemory,
        1,
        100,
        false,
    )
    .unwrap();
    assert!(matches!(
        RlimitCharge::acquire(
            child.clone(),
            0,
            RlimitResource::LockedMemory,
            1,
            100,
            false
        ),
        Err(NamespaceError::QuotaLimit)
    ));
    // 失败必须回滚 child 已成功的 increment，否则下面的完整额度会错误被旧失败占用。
    drop(first);
    let next =
        RlimitCharge::acquire(child.clone(), 0, RlimitResource::LockedMemory, 1, 1, false).unwrap();
    drop(next);
    let privileged =
        RlimitCharge::acquire(child, 0, RlimitResource::LockedMemory, 20, 0, true).unwrap();
    drop(privileged);
}

#[test]
fn signal_counts_are_distinct_from_locked_pages_and_refund_original_uid() {
    let user = namespace::boot();
    let locked = RlimitCharge::acquire(
        user.clone(),
        591201,
        RlimitResource::LockedMemory,
        2,
        2,
        false,
    )
    .unwrap();
    let signal = RlimitCharge::acquire(
        user.clone(),
        591201,
        RlimitResource::PendingSignals,
        1,
        1,
        false,
    )
    .unwrap();
    assert!(matches!(
        RlimitCharge::acquire(
            user.clone(),
            591201,
            RlimitResource::PendingSignals,
            1,
            1,
            false
        ),
        Err(NamespaceError::QuotaLimit)
    ));
    let other_uid = RlimitCharge::acquire(
        user.clone(),
        591202,
        RlimitResource::PendingSignals,
        1,
        1,
        false,
    )
    .unwrap();
    drop(signal);
    let next =
        RlimitCharge::acquire(user, 591201, RlimitResource::PendingSignals, 1, 1, false).unwrap();
    drop((locked, other_uid, next));
}

#[test]
fn realtime_signal_ancestor_limit_is_creation_time_snapshot_and_failure_unwinds_prefix() {
    namespace::boot();
    let mut authority = namespace::root_authority();
    authority.effective_uid = 591203;
    let child = UserNamespace::child(
        &authority,
        true,
        namespace::NamespaceRlimits {
            locked_memory: 100,
            pending_signals: 1,
        },
    )
    .unwrap();
    let first = RlimitCharge::acquire(
        child.clone(),
        0,
        RlimitResource::PendingSignals,
        1,
        100,
        false,
    )
    .unwrap();
    assert!(matches!(
        RlimitCharge::acquire(
            child.clone(),
            1,
            RlimitResource::PendingSignals,
            1,
            100,
            false
        ),
        Err(NamespaceError::QuotaLimit)
    ));
    drop(first);
    let retry =
        RlimitCharge::acquire(child, 1, RlimitResource::PendingSignals, 1, 1, false).unwrap();
    drop(retry);
}
