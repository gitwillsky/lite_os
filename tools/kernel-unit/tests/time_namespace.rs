#![feature(allocator_ext)]
#![allow(dead_code)]
//! 真实 TimeNamespace owner 的范围、冻结、收费和失败清理；clock 输入为固定 host scalar。
extern crate alloc;
#[path = "../src/allocation_failure.rs"]
mod allocation_failure;
#[path = "../../../kernel/src/fallible_tree/mod.rs"]
mod fallible_tree;
#[path = "../src/namespace_fixture.rs"]
mod namespace;
use namespace::{
    NamespaceAuthority, TimeClock, TimeOffset, TimeOffsetError, TimeOffsets, UserNamespace,
    UserResource,
};
use syscall_abi::capability::VALID_MASK;

fn authority() -> NamespaceAuthority {
    NamespaceAuthority {
        namespace: namespace::boot(),
        effective_uid: 0,
        effective_gid: 0,
        effective_capabilities: VALID_MASK,
    }
}
fn offset(seconds: i64, nanoseconds: u32) -> TimeOffset {
    TimeOffset {
        seconds,
        nanoseconds,
    }
}

#[test]
fn initial_identity_is_frozen_and_copy_reopens_the_actual_offsets_owner() {
    let caller = authority();
    let initial = namespace::initial_time();
    let reference = namespace::NamespaceReference::new(initial.clone());
    assert_eq!(reference.kind().flag(), 0x80);
    assert_eq!(reference.inode(), 0xefff_fffa);
    assert_eq!(
        initial.update(&[], (0, 0), &caller),
        Err(TimeOffsetError::Frozen)
    );
    let child = initial.copy_for(caller.namespace.clone(), 0).unwrap();
    child
        .update(
            &[
                (TimeClock::Monotonic, offset(-1, 250_000_000)),
                (TimeClock::Boottime, offset(300, 0)),
            ],
            (2_000_000_000, 2_000_000_000),
            &caller,
        )
        .unwrap();
    let values = child.snapshot();
    assert_eq!(
        values.project(TimeClock::Monotonic, 2_000_000_000),
        1_250_000_000
    );
    assert_eq!(
        values.project(TimeClock::Boottime, 2_000_000_000),
        302_000_000_000
    );
    assert_eq!(values.to_host(TimeClock::Boottime, 1), 0);
    assert_eq!(
        values.to_host(TimeClock::Monotonic, u64::MAX),
        i64::MAX as u64
    );
    child.enter();
    let grandchild = child.copy_for(caller.namespace.clone(), 0).unwrap();
    assert_eq!(grandchild.snapshot(), values);
    grandchild
        .update(
            &[(TimeClock::Monotonic, offset(0, 0))],
            (2_000_000_000, 2_000_000_000),
            &caller,
        )
        .unwrap();
    assert_eq!(child.snapshot(), values);
}
#[test]
fn mixed_update_rejects_invalid_or_out_of_range_without_partial_publication() {
    let caller = authority();
    let child = namespace::initial_time()
        .copy_for(caller.namespace.clone(), 0)
        .unwrap();
    for (bad, error) in [
        (offset(0, 1_000_000_000), TimeOffsetError::Invalid),
        (offset(i64::MAX, 0), TimeOffsetError::Range),
        (offset(-3, 0), TimeOffsetError::Range),
        (
            offset(i64::MAX / 1_000_000_000 / 2 + 1, 0),
            TimeOffsetError::Range,
        ),
    ] {
        assert_eq!(
            child.update(
                &[
                    (TimeClock::Monotonic, offset(100, 0)),
                    (TimeClock::Boottime, bad)
                ],
                (2_000_000_000, 2_000_000_000),
                &caller
            ),
            Err(error)
        );
        assert_eq!(child.snapshot(), TimeOffsets::ZERO);
    }
    let denied = NamespaceAuthority {
        effective_capabilities: 0,
        ..caller.clone()
    };
    assert_eq!(
        child.update(&[], (0, 0), &denied),
        Err(TimeOffsetError::Permission)
    );
    child.enter();
    assert_eq!(
        child.update(
            &[(TimeClock::Boottime, offset(i64::MAX, 0))],
            (0, 0),
            &caller
        ),
        Err(TimeOffsetError::Range)
    );
    assert_eq!(
        child.update(&[(TimeClock::Boottime, offset(1, 0))], (0, 0), &caller),
        Err(TimeOffsetError::Frozen)
    );
}
#[test]
fn quota_and_allocation_failures_release_every_actual_ancestor_charge() {
    let parent = authority();
    let user = UserNamespace::child(
        &parent,
        true,
        crate::namespace::NamespaceRlimits {
            locked_memory: u64::MAX,
            pending_signals: u64::MAX,
        },
    )
    .unwrap();
    let caller = NamespaceAuthority {
        namespace: user.clone(),
        ..parent
    };
    user.set_resource_limit(UserResource::Time, 1, &caller)
        .unwrap();
    for failed_allocation in 0..4 {
        allocation_failure::fail_after(failed_allocation);
        let result = namespace::initial_time().copy_for(user.clone(), 0);
        let failed = allocation_failure::was_consumed();
        assert_eq!(result.is_err(), failed);
        drop(result);
        let owned = namespace::initial_time().copy_for(user.clone(), 0).unwrap();
        assert!(namespace::initial_time().copy_for(user.clone(), 0).is_err());
        drop(owned);
    }
}
#[test]
fn enter_and_update_linearize_on_one_owner_lock() {
    let caller = authority();
    let child = namespace::initial_time()
        .copy_for(caller.namespace.clone(), 0)
        .unwrap();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let thread = {
        let child = child.clone();
        let barrier = barrier.clone();
        let caller = caller.clone();
        std::thread::spawn(move || {
            barrier.wait();
            child.update(
                &[
                    (TimeClock::Monotonic, offset(1, 0)),
                    (TimeClock::Boottime, offset(2, 0)),
                ],
                (0, 0),
                &caller,
            )
        })
    };
    barrier.wait();
    child.enter();
    let result = thread.join().unwrap();
    assert_eq!(
        child.snapshot(),
        if result.is_ok() {
            TimeOffsets {
                monotonic: offset(1, 0),
                boottime: offset(2, 0),
            }
        } else {
            assert_eq!(result, Err(TimeOffsetError::Frozen));
            TimeOffsets::ZERO
        }
    );
}
