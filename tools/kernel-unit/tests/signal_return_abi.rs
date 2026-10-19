//! 直接执行生产 rt_sigreturn syscall：区分 OOM、架构 fault metadata 与成功恢复。
#![allow(dead_code)]

use std::{
    cell::{Cell, RefCell},
    sync::Arc,
};

#[path = "../../../kernel/src/arch/aarch64/signal_action.rs"]
mod signal_action;
#[path = "../../../kernel/src/task/model/signal_info.rs"]
mod signal_info;

mod system {
    pub(crate) use super::signal_action::SignalActionBytes;
}

mod sync {
    pub(crate) enum WaitResult {
        OutOfMemory,
        Woken,
        Interrupted,
        TimedOut,
    }
}

mod task {
    use super::*;
    pub(crate) use signal_info::PendingSignal;
    #[derive(Clone, Copy)]
    pub(crate) enum SignalFrameRestoreError {
        OutOfMemory,
        Invalid { address: Option<usize> },
    }
    pub(crate) struct SignalAction {
        pub handler: usize,
        pub flags: usize,
        pub restorer: usize,
        pub mask: u64,
    }
    pub(crate) struct SignalStack {
        pub sp: usize,
        pub flags: i32,
        pub size: usize,
    }
    pub(crate) enum SignalStackError {
        Active,
        InvalidFlags,
        TooSmall,
    }
    pub(crate) enum SignalSendError {
        InvalidSignal,
        NotFound,
        Permission,
        Again,
    }
    pub(crate) enum SignalWaitError {
        Again,
        Interrupted,
        OutOfMemory,
    }

    pub(crate) struct Task {
        pub result: Result<usize, SignalFrameRestoreError>,
        pub restart_pending: Cell<bool>,
        pub lookup_oom: bool,
        pub looked_up: RefCell<Option<Option<usize>>>,
        pub signal: RefCell<Option<(usize, PendingSignal)>>,
    }
    thread_local! { pub static CURRENT: RefCell<Option<Arc<Task>>> = const { RefCell::new(None) }; }
    pub(crate) fn current_task() -> Option<Arc<Task>> {
        CURRENT.with(|slot| slot.borrow().clone())
    }
    pub(crate) fn exit_current_group_by_signal(signal: usize) -> ! {
        // Fatal transfer 之前必须释放 syscall 的 TCB Arc，防止退出路径泄漏 mm/Thread。
        assert_eq!(Arc::strong_count(&current_task().unwrap()), 3);
        panic!("group fatal {signal}");
    }
    impl Task {
        pub(crate) fn resolve_pid(&self, _: usize) -> Option<usize> {
            unreachable!(
                "signal number lookup belongs to PID/kill fixtures; this fixture exercises sigreturn"
            )
        }
        pub(crate) fn clear_syscall_restart(&self) {
            self.restart_pending.set(false);
        }
        pub fn restore_signal_frame(&self) -> Result<usize, SignalFrameRestoreError> {
            self.result
        }
        pub fn signal_frame_fault(&self, address: Option<usize>) -> Result<PendingSignal, ()> {
            *self.looked_up.borrow_mut() = Some(address);
            if self.lookup_oom {
                return Err(());
            }
            Ok(PendingSignal::synchronous_fault(
                if address.is_some() {
                    syscall_abi::signal::SEGV_ACCERR
                } else {
                    syscall_abi::signal::SI_KERNEL
                },
                address.unwrap_or(0),
            ))
        }
        pub fn queue_synchronous_fault(
            &self,
            number: usize,
            info: PendingSignal,
        ) -> Result<(), ()> {
            *self.signal.borrow_mut() = Some((number, info));
            Ok(())
        }
        pub fn copy_from_user(&self, _: usize, _: &mut [u8]) -> Result<(), ()> {
            unimplemented!()
        }
        pub fn copy_to_user(&self, _: usize, _: &[u8]) -> Result<(), ()> {
            unimplemented!()
        }
        pub fn signal_action(&self, _: usize, _: Option<SignalAction>) -> Result<SignalAction, ()> {
            unimplemented!()
        }
        pub fn signal_mask(&self, _: usize, _: Option<u64>) -> Result<u64, ()> {
            unimplemented!()
        }
        pub fn signal_stack(
            &self,
            _: Option<SignalStack>,
        ) -> Result<SignalStack, SignalStackError> {
            unimplemented!()
        }
        pub fn begin_signal_suspend(&self, _: u64) {
            unimplemented!()
        }
        pub fn pending_signal_set(&self) -> u64 {
            unimplemented!()
        }
        pub fn caught_signal_set(&self, _: u64) -> u64 {
            unimplemented!()
        }
        pub fn restore_temporary_signal_mask(&self) -> Option<u64> {
            unimplemented!()
        }
    }
    pub(crate) fn send_process_signal(_: i32, _: usize) -> Result<(), SignalSendError> {
        unimplemented!()
    }
    pub(crate) fn send_thread_signal(_: usize, _: usize, _: usize) -> Result<(), SignalSendError> {
        unimplemented!()
    }
    pub(crate) fn send_tid_signal(_: usize, _: usize) -> Result<(), SignalSendError> {
        unimplemented!()
    }
    pub(crate) fn send_queued_process_signal(
        _: i32,
        _: usize,
        _: PendingSignal,
    ) -> Result<(), SignalSendError> {
        unimplemented!()
    }
    pub(crate) fn send_queued_thread_signal(
        _: i32,
        _: i32,
        _: usize,
        _: PendingSignal,
    ) -> Result<(), SignalSendError> {
        unimplemented!()
    }
    pub(crate) fn wait_for_signal(
        _: u64,
        _: Option<u64>,
    ) -> Result<(usize, PendingSignal), SignalWaitError> {
        unimplemented!()
    }
    pub(crate) fn wait_for_signal_delivery(_: u64) -> crate::sync::WaitResult {
        unimplemented!()
    }
}

#[path = "../../../kernel/src/syscall"]
mod syscall {
    pub(crate) use crate::timer;
    pub(crate) mod signal;
    pub(crate) use syscall_abi::errno;
}
use syscall::signal;
pub(crate) mod timer {
    pub(crate) struct TimeSpec {
        pub tv_sec: i64,
        pub tv_nsec: i64,
    }
    pub(crate) fn decode_timespec(_: &[u8]) -> TimeSpec {
        unimplemented!()
    }
    pub(crate) fn timespec_ns(_: TimeSpec) -> Result<u64, isize> {
        unimplemented!()
    }
    pub(crate) fn get_time_ns() -> u64 {
        unimplemented!()
    }
}

#[allow(
    clippy::arc_with_non_send_sync,
    reason = "TLS fixture only observes Arc lifetime on one host thread"
)]
fn install(
    result: Result<usize, task::SignalFrameRestoreError>,
    lookup_oom: bool,
) -> Arc<task::Task> {
    let task = Arc::new(task::Task {
        result,
        restart_pending: Cell::new(true),
        lookup_oom,
        looked_up: RefCell::new(None),
        signal: RefCell::new(None),
    });
    task::CURRENT.with(|slot| *slot.borrow_mut() = Some(task.clone()));
    task
}

#[test]
fn restored_return_register_is_preserved_without_fault_lookup() {
    let task = install(Ok(0x123456), false);
    assert_eq!(signal::sys_rt_sigreturn(), 0x123456);
    assert!(!task.restart_pending.get());
    assert!(task.looked_up.borrow().is_none());
    assert!(task.signal.borrow().is_none());
}

#[test]
fn bad_frame_forwards_failure_stage_address_and_forces_sigsegv() {
    for address in [None, Some(0x123456)] {
        let task = install(
            Err(task::SignalFrameRestoreError::Invalid { address }),
            false,
        );
        assert_eq!(signal::sys_rt_sigreturn(), 0);
        assert!(!task.restart_pending.get());
        assert_eq!(*task.looked_up.borrow(), Some(address));
        let (number, info) = task.signal.borrow_mut().take().unwrap();
        assert_eq!(number, syscall_abi::signal::SIGSEGV);
        assert!(info.is_forced_fault());
        assert_eq!(
            &info.encode(number)[16..24],
            &(address.unwrap_or(0) as u64).to_ne_bytes()
        );
    }
}

#[test]
fn copyin_and_fault_lookup_oom_kill_without_sigsegv_or_arc_leak() {
    for (result, lookup_oom) in [
        (Err(task::SignalFrameRestoreError::OutOfMemory), false),
        (
            Err(task::SignalFrameRestoreError::Invalid { address: Some(1) }),
            true,
        ),
    ] {
        let task = install(result, lookup_oom);
        let failure =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(signal::sys_rt_sigreturn))
                .unwrap_err();
        assert_eq!(failure.downcast_ref::<String>().unwrap(), "group fatal 9");
        assert!(task.signal.borrow().is_none());
        assert_eq!(Arc::strong_count(&task), 2);
        assert!(!task.restart_pending.get());
        if !lookup_oom {
            assert!(task.looked_up.borrow().is_none());
        }
    }
}
