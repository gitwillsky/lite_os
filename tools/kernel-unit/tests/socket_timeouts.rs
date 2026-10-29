#![allow(dead_code)]
//! 直接加载实际 socket timeout owner 与 native codec；scheduler cadence 是显式边界输入。
use syscall_abi::errno;

#[path = "../../../kernel/src/net"]
mod net {
    pub(crate) mod timeouts;
    pub(crate) use timeouts::{SocketIoDirection, SocketTimeout};
    pub(crate) struct Owner(timeouts::SocketTimeouts);
    impl Owner {
        pub(crate) fn new() -> Self {
            Self(timeouts::SocketTimeouts::new())
        }
        pub(crate) fn set(&self, direction: SocketIoDirection, value: SocketTimeout) {
            self.0.set(direction, value);
        }
        pub(crate) fn get(&self, direction: SocketIoDirection) -> SocketTimeout {
            self.0.get(direction)
        }
        pub(crate) fn inherited(&self) -> Self {
            let child = Self::new();
            child.0.initialize(self.0.snapshot());
            child
        }
    }
}
#[path = "../../../kernel/src/syscall/socket/timeout.rs"]
mod timeout;
use net::{
    SocketIoDirection::{Receive, Send},
    SocketTimeout,
};
fn timeval(seconds: i64, micros: i64) -> [u8; 16] {
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&seconds.to_ne_bytes());
    bytes[8..].copy_from_slice(&micros.to_ne_bytes());
    bytes
}
#[test]
fn native_zero_and_negative_seconds_have_distinct_wait_policies_with_zero_getter() {
    let infinite = timeout::decode(timeval(0, 0)).unwrap();
    let immediate = timeout::decode(timeval(-1, 0)).unwrap();
    assert_eq!(infinite.deadline(17), None);
    assert_eq!(immediate.deadline(17), Some(17));
    assert_eq!(timeout::encode(infinite), timeval(0, 0));
    assert_eq!(timeout::encode(immediate), timeval(0, 0));
    for seconds in [i64::MIN, -1, 0, i64::MAX] {
        for micros in [-1, 1_000_000, i64::MAX] {
            assert_eq!(timeout::decode(timeval(seconds, micros)), Err(-errno::EDOM));
        }
    }
}
#[test]
fn finite_policy_rounds_up_to_actual_scheduler_tick_and_saturates_both_conversions() {
    let finite = timeout::decode(timeval(2, 1)).unwrap();
    assert_eq!(timeout::encode(finite), timeval(2, 10_000));
    assert_eq!(finite.deadline(7), Some(2_010_000_007));
    assert_eq!(
        timeout::decode(timeval(i64::MAX, 999_999))
            .unwrap()
            .deadline(1),
        Some(i64::MAX as u64)
    );
    assert_eq!(finite.deadline(u64::MAX), Some(i64::MAX as u64));
}
#[test]
fn dup_shares_actual_policy_while_frozen_wait_and_accepted_owner_are_independent() {
    let owner = std::sync::Arc::new(net::Owner::new());
    let duplicate = owner.clone();
    owner.set(Receive, timeout::decode(timeval(0, 50_000)).unwrap());
    let frozen = owner.get(Receive).deadline(100);
    let accepted = owner.inherited();
    duplicate.set(Receive, SocketTimeout::Infinite);
    duplicate.set(Send, SocketTimeout::finite(0));
    assert_eq!(owner.get(Receive).deadline(200), None);
    assert_eq!(owner.get(Send).deadline(200), Some(200));
    assert_eq!(frozen, Some(50_000_100));
    assert_eq!(accepted.get(Receive).deadline(100), frozen);
    assert_eq!(accepted.get(Send), SocketTimeout::Infinite);
}

pub(crate) mod execution {
    pub(crate) mod timer {
        pub(crate) const SCHEDULER_TICK_NS: u64 = 10_000_000;
    }
}
