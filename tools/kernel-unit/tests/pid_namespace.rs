#![feature(allocator_ext)]
#![allow(dead_code)]
//! 真实 PID namespace allocation/lifecycle/authority owner；Task graph integration 单独验证。
extern crate alloc;
#[path = "../src/allocation_failure.rs"]
mod allocation_failure;
#[path = "../../../kernel/src/fallible_tree/mod.rs"]
mod fallible_tree;
#[path = "../src/namespace_fixture.rs"]
mod namespace;
use namespace::{
    NamespaceAuthority, NamespaceError, PidAllocationError, PidKey, PidNamespace, PidRegistration,
    PreparedPid, UserNamespace, UserResource,
};
use syscall_abi::capability::{Capability, VALID_MASK};

fn caller() -> NamespaceAuthority {
    NamespaceAuthority {
        namespace: namespace::boot(),
        effective_uid: 0,
        effective_gid: 0,
        effective_capabilities: VALID_MASK,
    }
}
fn child() -> alloc::sync::Arc<PidNamespace> {
    let authority = caller();
    namespace::initial_pid()
        .child(authority.namespace, 0)
        .unwrap()
}
struct Published {
    number: u32,
    _registration: PidRegistration,
}
fn publish(namespace: &alloc::sync::Arc<PidNamespace>, key: usize) -> Published {
    let mut prepared = PreparedPid::try_new(
        key,
        namespace.clone(),
        &[],
        &crate::namespace::root_authority(),
    )
    .unwrap();
    let number = prepared.identity().number_in(namespace);
    prepared.publish().unwrap();
    Published {
        number,
        _registration: prepared.take_registration(),
    }
}

#[test]
fn unpublished_reservation_is_invisible_and_failed_first_fork_retries_pid_one() {
    let namespace = child();
    let first = namespace.reserve(PidKey::from_identity(100), None).unwrap();
    assert_eq!(first.number(), 1);
    assert_eq!(namespace.lookup(1), None);
    assert_eq!(namespace.next_member(0), None);
    assert!(matches!(
        namespace.reserve(PidKey::from_identity(101), None),
        Err(PidAllocationError::Invalid)
    ));
    drop(first);
    let init = publish(&namespace, 102);
    assert_eq!(init.number, 1);
    assert_eq!(namespace.reaper(), Some(PidKey::from_identity(102)));
    let member = publish(&namespace, 103);
    assert_eq!(member.number, 2);
    drop(member);
    drop(init);
}

#[test]
fn init_exit_revokes_both_prepared_and_future_allocation() {
    let namespace = child();
    let init = publish(&namespace, 100);
    assert_eq!(init.number, 1);
    let mut pending = PreparedPid::try_new(
        101,
        namespace.clone(),
        &[],
        &crate::namespace::root_authority(),
    )
    .unwrap();
    namespace.disable(PidKey::from_identity(100), None);
    assert!(!namespace.allows_creation());
    assert!(matches!(pending.publish(), Err(PidAllocationError::Dead)));
    drop(pending);
    assert_eq!(namespace.lookup(2), None);
    assert!(matches!(
        namespace.reserve(PidKey::from_identity(102), None),
        Err(PidAllocationError::Dead)
    ));
    assert_eq!(
        namespace.next_member(0),
        Some((1, PidKey::from_identity(100)))
    );
    allocation_failure::fail_next();
    assert!(matches!(
        namespace.reserve(PidKey::from_identity(103), None),
        Err(PidAllocationError::OutOfMemory)
    ));
    assert!(allocation_failure::was_consumed());
    drop(init);
}

#[test]
fn cyclic_reuse_preserves_reserved_floor_and_replaces_stable_association() {
    let namespace = child();
    namespace.set_limit(301, &caller()).unwrap();
    let mut members = Vec::new();
    for number in 1..=300 {
        let member = publish(&namespace, 1000 + number as usize);
        assert_eq!(member.number, number);
        members.push(Some(member));
    }
    assert!(matches!(
        namespace.reserve(PidKey::from_identity(2000), None),
        Err(PidAllocationError::Exhausted)
    ));
    drop(members[9].take());
    assert!(matches!(
        namespace.reserve(PidKey::from_identity(2000), None),
        Err(PidAllocationError::Exhausted)
    ));
    drop(members[299].take());
    let reused = publish(&namespace, 2000);
    assert_eq!(reused.number, 300);
    assert_eq!(namespace.lookup(300), Some(PidKey::from_identity(2000)));
    assert_eq!(namespace.lookup(10), None);
    drop(members);
    drop(reused);
}

#[test]
fn lower_owner_enforces_ancestry_depth_actual_authority_and_quota() {
    let parent = caller();
    let user = UserNamespace::child(
        &parent,
        true,
        crate::namespace::NamespaceRlimits {
            locked_memory: u64::MAX,
            pending_signals: u64::MAX,
        },
    )
    .unwrap();
    let authority = NamespaceAuthority {
        namespace: user.clone(),
        ..parent.clone()
    };
    user.set_resource_limit(UserResource::Pid, 1, &authority)
        .unwrap();
    let namespace = namespace::initial_pid().child(user.clone(), 0).unwrap();
    assert!(matches!(
        namespace::initial_pid().child(user.clone(), 0),
        Err(NamespaceError::QuotaLimit)
    ));
    assert!(matches!(
        namespace.child(parent.namespace.clone(), 0),
        Err(NamespaceError::Invalid)
    ));
    let denied = NamespaceAuthority {
        effective_capabilities: VALID_MASK & !Capability::SYS_ADMIN.mask(),
        ..authority.clone()
    };
    assert_eq!(
        namespace.set_limit(1000, &denied),
        Err(NamespaceError::Permission)
    );
    assert_eq!(
        namespace.set_limit(300, &authority),
        Err(NamespaceError::Invalid)
    );
    namespace.set_limit(1000, &authority).unwrap();
    assert_eq!(namespace.limit(), 1000);
    drop(namespace);
    user.set_resource_limit(UserResource::Pid, 40, &authority)
        .unwrap();
    let mut current = namespace::initial_pid();
    for level in 1..=32 {
        current = current.child(user.clone(), 0).unwrap();
        assert_eq!(current.level(), level);
    }
    assert!(matches!(
        current.child(user, 0),
        Err(NamespaceError::NestingLimit)
    ));
}

#[test]
fn allocation_failure_returns_all_charges_before_retry() {
    let parent = caller();
    let user = UserNamespace::child(
        &parent,
        true,
        crate::namespace::NamespaceRlimits {
            locked_memory: u64::MAX,
            pending_signals: u64::MAX,
        },
    )
    .unwrap();
    let authority = NamespaceAuthority {
        namespace: user.clone(),
        ..parent
    };
    user.set_resource_limit(UserResource::Pid, 1, &authority)
        .unwrap();
    for step in 0..4 {
        allocation_failure::fail_after(step);
        let result = namespace::initial_pid().child(user.clone(), 0);
        let consumed = allocation_failure::was_consumed();
        assert_eq!(result.is_err(), consumed);
        drop(result);
        let actual = namespace::initial_pid().child(user.clone(), 0).unwrap();
        assert!(namespace::initial_pid().child(user.clone(), 0).is_err());
        drop(actual);
    }
}

#[test]
fn shutdown_waits_for_pending_reservation_and_weak_wake_cannot_keep_event_alive() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Wake(AtomicUsize);
    impl namespace::PidExitWake for Wake {
        fn signal(&self) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }
    let namespace = child();
    let init = publish(&namespace, 500);
    let pending = namespace.reserve(PidKey::from_identity(501), None).unwrap();
    let event = alloc::sync::Arc::new(Wake(AtomicUsize::new(0)));
    let erased: alloc::sync::Arc<dyn namespace::PidExitWake> = event.clone();
    let weak = alloc::sync::Arc::downgrade(&erased);
    namespace.disable(PidKey::from_identity(500), Some(weak.clone()));
    assert!(!namespace.reaper_only(1));
    drop(pending);
    assert!(namespace.reaper_only(1));
    assert_eq!(event.0.load(Ordering::Relaxed), 1);
    drop(erased);
    drop(event);
    assert!(weak.upgrade().is_none());
    drop(init);
}
