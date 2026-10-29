#![feature(allocator_ext)]
#![allow(dead_code)]
//! 实际 native control import/codec；task seam 只生产 user-copy、fault 与调用次数。
extern crate alloc;
#[path = "../src/allocation_failure.rs"]
mod allocation_failure;
#[path = "../../../kernel/src/fallible_tree/mod.rs"]
mod fallible_tree;
#[path = "../../../kernel/src/syscall/user_iovec/input_staging.rs"]
mod input_staging;
#[path = "../src/namespace_fixture.rs"]
mod namespace;
#[path = "../src/socket_option_memory_fixture.rs"]
mod net;
mod syscall {
    pub(crate) mod user_iovec {
        pub(crate) use crate::input_staging::UserInputStaging;
    }
}

#[path = "../../../kernel/src/syscall/socket/control_list.rs"]
mod control_list;
use control_list::{ControlList, ControlSnapshot, ImportError};
fn cmsg(data: &[u8], padding: usize) -> Vec<u8> {
    let mut bytes = vec![0; 16 + data.len() + padding];
    bytes[..8].copy_from_slice(&(16 + data.len()).to_ne_bytes());
    bytes[8..12].copy_from_slice(&1i32.to_ne_bytes());
    bytes[12..16].copy_from_slice(&1i32.to_ne_bytes());
    bytes[16..16 + data.len()].copy_from_slice(data);
    bytes
}
#[test]
fn one_copy_freezes_header_and_fd_bytes_even_when_user_memory_changes_immediately() {
    let socket = net::endpoint();
    for count in [1, 8] {
        let data: Vec<_> = (0..count).flat_map(|fd: i32| fd.to_ne_bytes()).collect();
        let bytes = cmsg(&data, 0);
        let task = crate::execution::task::TaskControlBlock::new(bytes.clone());
        task.mutate_after_copy.set(true);
        let snapshot = ControlSnapshot::import(&task, &socket, 1, bytes.len()).unwrap();
        assert_eq!(task.copies.get(), 1);
        assert_eq!(snapshot.bytes(), &bytes);
        let mut list = ControlList::new(snapshot.bytes());
        let message = list.next().unwrap().unwrap();
        assert_eq!((message.level, message.kind), (1, 1));
        assert_eq!(message.data, &data);
        assert!(list.next().is_none());
    }
}
#[test]
fn short_controls_use_stack_and_large_allocation_failure_never_reads_user_memory() {
    let socket = net::endpoint();
    let bytes = cmsg(&3i32.to_ne_bytes(), 0);
    let task = crate::execution::task::TaskControlBlock::new(bytes.clone());
    allocation_failure::fail_next();
    let snapshot = ControlSnapshot::import(&task, &socket, 1, bytes.len()).unwrap();
    assert!(!allocation_failure::was_consumed());
    assert_eq!(snapshot.bytes(), &bytes);
    allocation_failure::fail_next();
    let failed = ControlSnapshot::import(&task, &socket, 1, 40);
    assert!(allocation_failure::was_consumed());
    assert!(matches!(failed, Err(ImportError::NoBuffer)));
    assert_eq!(task.copies.get(), 1);
}
#[test]
fn oversized_native_length_is_rejected_before_allocation_or_user_access() {
    let socket = net::endpoint();
    let task = crate::execution::task::TaskControlBlock::new(Vec::new());
    allocation_failure::fail_next();
    let result = ControlSnapshot::import(&task, &socket, 0, i32::MAX as usize + 1);
    assert!(!allocation_failure::was_consumed());
    assert!(matches!(result, Err(ImportError::NoBuffer)));
    assert_eq!(task.copies.get(), 0);
}
#[test]
fn zero_control_ignores_pointer_and_partial_copy_fault_publishes_no_snapshot() {
    let socket = net::endpoint();
    let task = crate::execution::task::TaskControlBlock::new(vec![7; 64]);
    assert!(
        ControlSnapshot::import(&task, &socket, 0, 0)
            .unwrap()
            .bytes()
            .is_empty()
    );
    assert_eq!(task.copies.get(), 0);
    task.fault.set(true);
    assert!(matches!(
        ControlSnapshot::import(&task, &socket, 1, 64),
        Err(ImportError::CopyFault)
    ));
    assert_eq!(task.copies.get(), 1);
    assert!(socket.memory.reserve(128 * 1024 - 1).is_some());
}
#[test]
fn heap_snapshot_holds_requested_bytes_until_drop_and_budget_rejects_before_copy() {
    let socket = net::endpoint();
    let task = crate::execution::task::TaskControlBlock::new(vec![7; 65_536]);
    let first = ControlSnapshot::import(&task, &socket, 1, 65_535).unwrap();
    let second = ControlSnapshot::import(&task, &socket, 1, 65_536).unwrap();
    assert_eq!(task.copies.get(), 2);
    assert!(matches!(
        ControlSnapshot::import(&task, &socket, 1, 37),
        Err(ImportError::NoBuffer)
    ));
    assert_eq!(task.copies.get(), 2);
    // 栈路径不使用 heap budget，已有堆申请几乎占满仍能完整 copy。
    let inline = ControlSnapshot::import(&task, &socket, 1, 36).unwrap();
    assert_eq!(inline.bytes().len(), 36);
    assert_eq!(task.copies.get(), 3);
    drop(first);
    let third = ControlSnapshot::import(&task, &socket, 1, 65_535).unwrap();
    assert!(socket.memory.reserve(1).is_none());
    drop(second);
    drop(third);
    assert!(socket.memory.reserve(128 * 1024 - 1).is_some());
    let before = task.copies.get();
    for length in [128 * 1024, 128 * 1024 + 1] {
        assert!(matches!(
            ControlSnapshot::import(&task, &socket, 0, length),
            Err(ImportError::NoBuffer)
        ));
    }
    assert_eq!(task.copies.get(), before);
}
#[test]
fn repeated_allocation_failures_and_partial_copy_faults_refund_the_actual_owner() {
    let socket = net::endpoint();
    let task = crate::execution::task::TaskControlBlock::new(vec![7; 64]);
    for _ in 0..8 {
        allocation_failure::fail_next();
        assert!(matches!(
            ControlSnapshot::import(&task, &socket, 1, 64),
            Err(ImportError::NoBuffer)
        ));
        assert!(allocation_failure::was_consumed());
        task.fault.set(true);
        assert!(matches!(
            ControlSnapshot::import(&task, &socket, 1, 64),
            Err(ImportError::CopyFault)
        ));
        task.fault.set(false);
        assert!(socket.memory.reserve(128 * 1024 - 1).is_some());
    }
    assert_eq!(task.copies.get(), 8);
}
#[test]
fn native_next_header_accepts_empty_rights_short_payload_and_incomplete_tail_padding() {
    for payload in [vec![], vec![1, 2, 3], vec![5, 0, 0, 0, 255]] {
        for padding in 0..8 {
            let bytes = cmsg(&payload, padding);
            let mut messages = ControlList::new(&bytes);
            assert_eq!(messages.next().unwrap().unwrap().data, &payload);
            assert!(messages.next().is_none());
        }
    }
    assert!(ControlList::new(&[255; 15]).next().is_none());
}
#[test]
fn malformed_complete_second_header_returns_error_without_reparsing_first_message() {
    let mut bytes = cmsg(&4i32.to_ne_bytes(), 4);
    bytes.extend_from_slice(&[0; 16]);
    let mut messages = ControlList::new(&bytes);
    assert_eq!(messages.next().unwrap().unwrap().data, &4i32.to_ne_bytes());
    assert!(messages.next().unwrap().is_err());
    assert!(messages.next().is_none());
    for length in [0usize, 15, 17, usize::MAX] {
        let mut bytes = [0; 16];
        bytes[..8].copy_from_slice(&length.to_ne_bytes());
        assert!(ControlList::new(&bytes).next().unwrap().is_err());
    }
}

pub(crate) mod execution {
    pub(crate) mod task {
        use core::mem::MaybeUninit;
        use std::cell::{Cell, RefCell};
        pub(crate) struct TaskControlBlock {
            pub(crate) bytes: RefCell<Vec<u8>>,
            pub(crate) copies: Cell<usize>,
            pub(crate) fault: Cell<bool>,
            pub(crate) mutate_after_copy: Cell<bool>,
        }
        impl TaskControlBlock {
            pub(crate) fn new(bytes: Vec<u8>) -> Self {
                Self {
                    bytes: RefCell::new(bytes),
                    copies: Cell::new(0),
                    fault: Cell::new(false),
                    mutate_after_copy: Cell::new(false),
                }
            }
            pub(crate) fn copy_from_user(
                &self,
                pointer: usize,
                output: &mut [u8],
            ) -> Result<(), ()> {
                self.copies.set(self.copies.get() + 1);
                if pointer == 0 || self.fault.get() {
                    return Err(());
                }
                let mut input = self.bytes.borrow_mut();
                let source = input.get(..output.len()).ok_or(())?;
                output.copy_from_slice(source);
                if self.mutate_after_copy.get() {
                    input.fill(0xff);
                }
                Ok(())
            }
            pub(crate) fn copy_from_user_uninit(
                &self,
                pointer: usize,
                output: &mut [MaybeUninit<u8>],
            ) -> Result<(), ()> {
                self.copies.set(self.copies.get() + 1);
                if pointer == 0 {
                    return Err(());
                }
                let mut input = self.bytes.borrow_mut();
                let source = input.get(..output.len()).ok_or(())?;
                if self.fault.get() {
                    for (target, byte) in output.iter_mut().zip(source.iter()).take(3) {
                        target.write(*byte);
                    }
                    return Err(());
                }
                for (target, byte) in output.iter_mut().zip(source.iter()) {
                    target.write(*byte);
                }
                if self.mutate_after_copy.get() {
                    input.fill(0xff);
                }
                Ok(())
            }
        }
    }
}
