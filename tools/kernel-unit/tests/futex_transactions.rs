#![feature(allocator_ext)]
#![allow(dead_code, unused_imports)]
//! actual source transaction、跨 shard node 移交、native atomic codec；不实现测试版 wait queue。
extern crate alloc;
#[path = "../src/allocation_failure.rs"]
mod allocation_failure;
include!("../src/futex_owner_fixture.rs");
#[path = "../src/interruptible_event_target.rs"]
mod scheduler_target;
use crate::execution::task::process_table as owner;
use core::sync::atomic::{AtomicU32, Ordering};
use memory::{FutexKey, UserAtomicU32Op};
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
fn key(address: usize) -> FutexKey {
    FutexKey::Private {
        address_space: 1,
        address,
    }
}
fn encoded(operation: u32, operand: i32, comparison: u32, argument: i32) -> u32 {
    operation << 28 | comparison << 24 | (operand as u32 & 4095) << 12 | (argument as u32 & 4095)
}
#[test]
fn actual_rmw_uses_signed_operands_wrapping_add_and_old_signed_comparison() {
    for (operator, start, operand, result) in [
        (0, 7, -2, u32::MAX - 1),
        (1, u32::MAX, 2, 1),
        (2, 0x12, 0x20, 0x32),
        (3, 0xff, 0xf, 0xf0),
        (4, 0x12, 0x23, 0x31),
    ] {
        let word = AtomicU32::new(start);
        let op = wake_op::WakeOperation::decode(encoded(operator, operand, 1, -1));
        assert_eq!(memory::update(&word, op.update.unwrap()), start);
        assert_eq!(word.load(Ordering::SeqCst), result);
    }
    for shift in [-1, 0, 31, 32, 2047, -2048] {
        let op = wake_op::WakeOperation::decode(encoded(8, shift, 0, 0));
        assert_eq!(
            op.update,
            Some(UserAtomicU32Op::Set(1 << (shift as u32 & 31)))
        );
    }
    for old in [-3i32, -2, -1, 0, i32::MAX, i32::MIN] {
        for (comparison, expected) in [
            old == -2,
            old != -2,
            old < -2,
            old >= -2,
            old <= -2,
            old > -2,
        ]
        .into_iter()
        .enumerate()
        {
            assert_eq!(
                wake_op::WakeOperation::decode(encoded(0, 0, comparison as u32, -2))
                    .compare(old as u32),
                Ok(expected)
            );
        }
    }
    assert!(
        wake_op::WakeOperation::decode(encoded(7, 1, 0, 0))
            .update
            .is_none()
    );
    assert_eq!(
        wake_op::WakeOperation::decode(encoded(1, 1, 15, 0)).compare(0),
        Err(crate::execution::task::FutexWaitError::Unsupported)
    );
}
#[test]
fn requeue_preserves_actual_bitset_deadline_and_same_key_count_without_allocation() {
    let _serial = SERIAL.lock().unwrap();
    let source = key(0x1000);
    let target = key(0x1004);
    let pending = owner::wait(source, 2, Some(100), 10);
    allocation_failure::fail_next();
    assert_eq!(owner::requeue(source, source, 1), 1);
    assert!(!allocation_failure::was_consumed());
    allocation_failure::fail_next();
    assert_eq!(owner::requeue(source, target, 1), 1);
    assert!(!allocation_failure::was_consumed());
    assert_eq!(owner::wake(source, 1, u32::MAX), 0);
    assert_eq!(owner::wake(target, 1, 1), 0);
    assert!(!owner::expire(99));
    assert!(owner::expire(100));
    assert_eq!(
        pending.arm(),
        Err(crate::execution::sync::WaitResult::TimedOut)
    );
    owner::assert_empty();
}
#[test]
fn actual_two_source_wake_consumes_each_wait_once_and_handles_identical_keys() {
    let _serial = SERIAL.lock().unwrap();
    for target in [key(0x2004), key(0x2000)] {
        let source = key(0x2000);
        let first = owner::wait(source, u32::MAX, None, 11);
        let second = owner::wait(target, u32::MAX, None, 12);
        let word = AtomicU32::new(0);
        owner::with_operation(&[source, target], |wake| {
            let old = memory::update(&word, UserAtomicU32Op::Add(1));
            assert_eq!(old, 0);
            assert_eq!(wake(source, 1, u32::MAX), 1);
            assert_eq!(wake(target, 1, u32::MAX), 1);
        });
        assert_eq!(first.arm(), Err(crate::execution::sync::WaitResult::Woken));
        assert_eq!(second.arm(), Err(crate::execution::sync::WaitResult::Woken));
        assert_eq!(owner::wake(source, 2, u32::MAX), 0);
    }
}
#[test]
fn actual_armed_claims_reuse_index_nodes_and_retire_without_allocation() {
    let _serial = SERIAL.lock().unwrap();
    let source = key(0x4000);
    let target = key(0x4004);
    assert!(owner::wait(source, u32::MAX, Some(100), 14).arm().is_ok());
    assert!(owner::wait(target, u32::MAX, Some(100), 15).arm().is_ok());
    allocation_failure::fail_next();
    owner::with_operation(&[source, target], |wake| {
        assert_eq!(wake(source, 1, u32::MAX), 1);
        assert_eq!(wake(target, 1, u32::MAX), 1);
    });
    assert!(!allocation_failure::was_consumed());
    owner::assert_empty();
}
#[test]
fn wait_publication_cannot_cross_actual_two_source_operation() {
    let _serial = SERIAL.lock().unwrap();
    let source = key(0x3000);
    let target = key(0x3004);
    let scheduler = scheduler_target::Target::new();
    let main_scheduler = scheduler_target::Target::new();
    let _binding = main_scheduler.bind();
    let thread = owner::with_operation(&[source, target], |wake| {
        let scheduler_for_thread = scheduler.clone();
        let thread = std::thread::spawn(move || {
            let _binding = scheduler_for_thread.bind();
            owner::wait(target, u32::MAX, None, 13)
        });
        scheduler.wait_until_sleeping();
        assert_eq!(
            wake(target, 1, u32::MAX),
            0,
            "operation excludes later publication"
        );
        thread
    });
    let waiting = thread.join().unwrap();
    assert_eq!(waiting.cancel(), None);
    owner::assert_empty();
}
