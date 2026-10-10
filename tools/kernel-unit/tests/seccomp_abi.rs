//! 生产 seccomp/prctl codec 与 security owner 集成；不复制 validator 或 errno policy。
#![feature(allocator_ext)]
#![allow(clippy::duplicate_mod)]

extern crate alloc;
#[allow(dead_code, unused_imports)]
#[path = "../../../kernel/src/fallible_tree/mod.rs"]
mod fallible_tree;
#[allow(dead_code, unused_imports)]
#[path = "../src/namespace_fixture.rs"]
mod namespace;

use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    mem::MaybeUninit,
    sync::Arc,
};
use syscall_abi::{errno, seccomp::*};
const INTERNAL_RESTART_SYS: isize = -512;

macro_rules! info { ($($args:tt)*) => { let _ = format_args!($($args)*); }; }
// Capability state/codec is exercised in capabilities + thread_credentials fixtures.
// These route-specific fixtures never invoke capability prctl.
mod capabilities {
    pub(crate) fn capability_error(_: ()) -> isize {
        unreachable!("capability route seam")
    }
}
#[allow(dead_code, unused_imports)]
#[path = "../src/seccomp_environment.rs"]
mod seccomp_environment;
use seccomp_environment::{file, fs, id, ipc};
mod arch {
    pub(crate) mod user {
        pub(crate) const AUDIT_ARCH: u32 = 0xc000_00b7;
    }
}

#[path = "seccomp/security.rs"]
mod model;
#[path = "../../../kernel/src/syscall/process_control.rs"]
pub(crate) mod process_control;
#[path = "../../../kernel/src/syscall/seccomp.rs"]
pub(crate) mod seccomp;
#[allow(dead_code, unused_imports)]
#[path = "../src/sync.rs"]
mod sync;
use model::seccomp as security;
#[path = "../../../kernel/src/task/model/signal_info.rs"]
#[allow(dead_code)]
mod signal_info;
#[path = "../../../kernel/src/syscall/user_iovec/input_staging.rs"]
#[allow(dead_code)]
mod user_iovec;
use syscall::SyscallOutcome;

pub(crate) struct TaskControlBlock {
    security: security::ThreadSecurityOwner,
    uid: Cell<u32>,
    signal: RefCell<Option<(usize, signal_info::PendingSignal)>>,
}
impl TaskControlBlock {
    fn tid_identity(&self) -> alloc::sync::Arc<namespace::PidIdentity> {
        unreachable!("policy/codec fixture does not create notifications")
    }
    // MM/dumpability 由 process_control + user_exit_memory fixtures 和真实 guest 验证。
    pub(crate) fn user_dumpable(&self) -> bool {
        unreachable!("dumpability route seam")
    }
    pub(crate) fn set_user_dumpable(&self, _: bool) {
        unreachable!("dumpability route seam")
    }
    fn thread(&self) -> &Self {
        self
    }
    fn tid(&self) -> usize {
        5
    }
    fn user_program_counter(&self) -> usize {
        0x5678
    }
    fn no_new_privileges(&self) -> bool {
        self.security.no_new_privileges()
    }
    fn enable_no_new_privileges(&self) {
        self.security.enable_no_new_privileges();
    }
    fn has_capability(&self, cap: syscall_abi::capability::Capability) -> bool {
        assert_eq!(cap, syscall_abi::capability::Capability::SYS_ADMIN);
        self.uid.get() == 0 // injected authorization; actual capability owner covered separately
    }
    fn capability_prctl(&self, _: u32, _: usize, _: [usize; 3]) -> Result<usize, ()> {
        unreachable!("capability route seam")
    }
    fn copy_from_user(&self, address: usize, output: &mut [u8]) -> Result<(), ()> {
        USERS.with_borrow(|users| {
            let bytes = users
                .get(&address)
                .filter(|bytes| bytes.len() >= output.len())
                .ok_or(())?;
            output.copy_from_slice(&bytes[..output.len()]);
            Ok(())
        })
    }
    fn copy_from_user_uninit(
        &self,
        address: usize,
        output: &mut [MaybeUninit<u8>],
    ) -> Result<(), ()> {
        USERS.with_borrow(|users| {
            let bytes = users
                .get(&address)
                .filter(|bytes| bytes.len() >= output.len())
                .ok_or(())?;
            for (slot, byte) in output.iter_mut().zip(bytes) {
                slot.write(*byte);
            }
            Ok(())
        })
    }
    fn copy_to_user(&self, address: usize, bytes: &[u8]) -> Result<(), ()> {
        USERS.with_borrow_mut(|users| {
            let target = users
                .get_mut(&address)
                .filter(|target| target.len() >= bytes.len())
                .ok_or(())?;
            target[..bytes.len()].copy_from_slice(bytes);
            Ok(())
        })
    }
    fn queue_synchronous_fault(
        &self,
        signal: usize,
        info: signal_info::PendingSignal,
    ) -> Result<(), ()> {
        *self.signal.borrow_mut() = Some((signal, info));
        Ok(())
    }
}
thread_local! {
    static INSTALL_FAILURE: Cell<Option<usize>> = const { Cell::new(None) };
    static LAST_SYNCHRONIZE: Cell<bool> = const { Cell::new(false) };
    static CURRENT: RefCell<Option<Arc<TaskControlBlock>>> = const { RefCell::new(None) };
    static USERS: RefCell<BTreeMap<usize, Vec<u8>>> = const { RefCell::new(BTreeMap::new()) };
}
mod task {
    pub(crate) enum SyscallRestartPolicy {
        WithRestartHandler,
    }
    pub(crate) use super::TaskControlBlock;
    pub(crate) use super::security::{SeccompDecision, SeccompError};
    pub(crate) use super::signal_info::PendingSignal;
    pub(crate) fn current_task() -> Option<std::sync::Arc<super::TaskControlBlock>> {
        super::CURRENT.with_borrow(|current| current.clone())
    }
    pub(crate) fn enable_strict_seccomp() -> Result<(), SeccompError> {
        current_task().unwrap().security.enable_strict()
    }
    pub(crate) fn install_seccomp_filter(
        bytes: &[u8],
        flags: u32,
    ) -> Result<Option<usize>, SeccompError> {
        super::LAST_SYNCHRONIZE.set(flags & syscall_abi::seccomp::FILTER_FLAG_TSYNC != 0);
        if let Some(tid) = super::INSTALL_FAILURE.get() {
            return Err(SeccompError::Unsynchronized(tid));
        }
        let current = current_task().unwrap();
        let program = super::security::ValidatedFilter::prepare(bytes, flags)?;
        let prepared = current.security.prepare_filter(&program)?;
        assert!(prepared.matches(&current.security));
        drop(prepared.publish(&current.security, false));
        Ok(None)
    }
    pub(crate) fn parent_death_signal(_: Option<usize>) -> Result<usize, ()> {
        Err(())
    }
    pub(crate) fn exit_current_thread_by_signal(signal: usize) -> ! {
        panic!("thread fatal {signal}")
    }
    pub(crate) fn exit_current_group_by_signal(signal: usize) -> ! {
        panic!("group fatal {signal}")
    }
}
mod system {
    pub(crate) fn audit_architecture() -> u32 {
        super::arch::user::AUDIT_ARCH
    }
}
pub(crate) use syscall::file_descriptor_error;
mod syscall {
    pub(crate) use syscall_abi::errno;
    pub(crate) enum SyscallOutcome {
        Return(isize),
        PreserveRegisters,
        Restart(crate::task::SyscallRestartPolicy),
    }
    pub(crate) fn file_descriptor_error(_: crate::file::FileDescriptorError) -> isize {
        unreachable!("policy-only descriptor errno seam")
    }
    pub(crate) use super::{process_control, seccomp};
}

fn current(uid: u32) -> Arc<TaskControlBlock> {
    // 只在 calling test thread 使用 Cell；生产 Thread ownership 已由真实 state owner 体现。
    #[allow(clippy::arc_with_non_send_sync)]
    let task = Arc::new(TaskControlBlock {
        security: security::ThreadSecurityOwner::default(),
        uid: Cell::new(uid),
        signal: RefCell::new(None),
    });
    INSTALL_FAILURE.set(None);
    LAST_SYNCHRONIZE.set(false);
    CURRENT.set(Some(task.clone()));
    USERS.with_borrow_mut(BTreeMap::clear);
    task
}
fn user(bytes: Vec<u8>) -> usize {
    USERS.with_borrow_mut(|users| {
        let address = (users.len() + 1) * 0x10000;
        users.insert(address, bytes);
        address
    })
}
fn filter(action: u32) -> usize {
    let mut instruction = vec![6, 0, 0, 0];
    instruction.extend(action.to_ne_bytes());
    let pointer = user(instruction);
    header(1, pointer)
}
fn header(length: u16, pointer: usize) -> usize {
    let mut bytes = vec![0; 16];
    bytes[..2].copy_from_slice(&length.to_ne_bytes());
    bytes[8..].copy_from_slice(&pointer.to_ne_bytes());
    user(bytes)
}

#[test]
fn filter_copy_permission_and_validation_have_linux_error_ordering() {
    use syscall::seccomp::sys_seccomp;
    let task = current(1000);
    assert_eq!(sys_seccomp(SET_MODE_FILTER, 0, 0), -errno::EFAULT);
    assert_eq!(
        sys_seccomp(SET_MODE_FILTER, 0, header(0, 0)),
        -errno::EINVAL
    );
    let invalid_pointer = header(1, 0);
    assert_eq!(
        sys_seccomp(SET_MODE_FILTER, 0, invalid_pointer),
        -errno::EACCES
    );
    task.enable_no_new_privileges();
    assert_eq!(
        sys_seccomp(SET_MODE_FILTER, 0, invalid_pointer),
        -errno::EFAULT
    );
    let invalid_program = header(1, user(vec![0; 8]));
    assert_eq!(
        sys_seccomp(SET_MODE_FILTER, 0, invalid_program),
        -errno::EINVAL
    );
    assert_eq!(task.seccomp_mode(), MODE_DISABLED);
    assert_eq!(sys_seccomp(SET_MODE_FILTER, 0, filter(RET_ERRNO | 13)), 0);
    assert_eq!(task.seccomp_mode(), MODE_FILTER);
    assert_eq!(
        task.evaluate_seccomp(0, [0; 6]),
        security::SeccompDecision::Errno(13)
    );
}

#[test]
fn seccomp_and_prctl_route_to_one_irreversible_owner() {
    use syscall::{process_control::sys_prctl, seccomp::sys_seccomp};
    let task = current(0);
    assert_eq!(sys_prctl(21, usize::MAX, [usize::MAX; 3]), 0);
    assert_eq!(sys_seccomp(SET_MODE_STRICT, 1, 0), -errno::EINVAL);
    assert_eq!(sys_seccomp(SET_MODE_STRICT, 0, 1), -errno::EINVAL);
    assert_eq!(sys_prctl(22, MODE_STRICT as usize, [usize::MAX; 3]), 0);
    assert_eq!(task.seccomp_mode(), MODE_STRICT);
    assert_eq!(
        sys_prctl(22, MODE_FILTER as usize, [filter(RET_ALLOW), 0, 0]),
        -errno::EINVAL
    );
    assert_eq!(task.security.clone().mode(), MODE_STRICT);
    current(0);
    assert_eq!(sys_seccomp(SET_MODE_FILTER, 0, filter(RET_ALLOW)), 0);
    assert_eq!(sys_prctl(21, 0, [0; 3]), 2);
    assert_eq!(sys_prctl(22, 0x1_0000_0001, [0; 3]), -errno::EINVAL);
    assert_eq!(
        sys_seccomp(SET_MODE_FILTER, 1 << 31, filter(RET_ALLOW)),
        -errno::EINVAL
    );
}

#[test]
fn action_query_rejects_non_actions_and_copies_only_a_u32() {
    use syscall::seccomp::sys_seccomp;
    current(1000);
    assert_eq!(sys_seccomp(GET_ACTION_AVAIL, 0, 0), -errno::EFAULT);
    for action in [
        RET_KILL_PROCESS,
        RET_KILL_THREAD,
        RET_TRAP,
        RET_ERRNO,
        RET_TRACE,
        RET_USER_NOTIF,
        RET_LOG,
        RET_ALLOW,
    ] {
        assert_eq!(
            sys_seccomp(GET_ACTION_AVAIL, 0, user(action.to_ne_bytes().to_vec())),
            0
        );
    }
    assert_eq!(
        sys_seccomp(
            GET_ACTION_AVAIL,
            0,
            user((RET_ERRNO | 1).to_ne_bytes().to_vec())
        ),
        -errno::EOPNOTSUPP
    );
    assert_eq!(sys_seccomp(GET_NOTIF_SIZES, 0, 0), -errno::EFAULT);
}

#[test]
fn trap_preserves_completion_and_delivers_the_production_sigsys_codec() {
    let task = current(0);
    assert_eq!(
        syscall::seccomp::sys_seccomp(SET_MODE_FILTER, 0, filter(RET_TRAP | 42)),
        0
    );
    assert!(matches!(
        syscall::seccomp::enforce(task.clone(), 221, [0; 6]),
        Some(syscall::SyscallOutcome::PreserveRegisters)
    ));
    let (signal, info) = task.signal.borrow_mut().take().unwrap();
    assert_eq!(signal, 31);
    let bytes = info.encode(signal);
    assert_eq!(&bytes[4..8], &42i32.to_ne_bytes());
    assert_eq!(&bytes[16..24], &0x5678u64.to_ne_bytes());
    assert_eq!(&bytes[24..28], &221i32.to_ne_bytes());
    assert_eq!(&bytes[28..32], &arch::user::AUDIT_ARCH.to_ne_bytes());
}

#[test]
fn errno_and_missing_observer_skip_syscalls_without_a_signal() {
    for (action, expected) in [
        (RET_ERRNO, 0),
        (RET_ERRNO | 0xffff, -4095),
        (RET_TRACE, -errno::ENOSYS),
        (RET_USER_NOTIF, -errno::ENOSYS),
    ] {
        let task = current(0);
        assert_eq!(
            syscall::seccomp::sys_seccomp(SET_MODE_FILTER, 0, filter(action)),
            0
        );
        match syscall::seccomp::enforce(task.clone(), 172, [0; 6]) {
            Some(syscall::SyscallOutcome::Return(value)) => assert_eq!(value, expected),
            _ => panic!("missing errno completion"),
        }
        assert!(task.signal.borrow().is_none());
    }
}

#[test]
fn fatal_consequences_release_the_transferred_tcb_before_exit() {
    for (action, expected) in [
        (RET_KILL_THREAD, "thread fatal 31"),
        (RET_KILL_PROCESS, "group fatal 31"),
    ] {
        let task = current(0);
        assert_eq!(
            syscall::seccomp::sys_seccomp(SET_MODE_FILTER, 0, filter(action)),
            0
        );
        let references = Arc::strong_count(&task);
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            syscall::seccomp::enforce(task.clone(), 172, [0; 6])
        }))
        .err()
        .expect("fatal consequence must not return");
        assert_eq!(
            panic.downcast_ref::<String>().map(String::as_str),
            Some(expected)
        );
        assert_eq!(Arc::strong_count(&task), references);
    }
}

#[test]
fn tsync_flags_project_positive_tid_or_esrch_without_reinterpreting_other_failures() {
    let task = current(0);
    assert_eq!(
        seccomp::sys_seccomp(SET_MODE_FILTER, FILTER_FLAG_TSYNC, filter(RET_ALLOW)),
        0
    );
    assert!(LAST_SYNCHRONIZE.get());
    INSTALL_FAILURE.set(Some(77));
    assert_eq!(
        seccomp::sys_seccomp(SET_MODE_FILTER, FILTER_FLAG_TSYNC, filter(RET_ALLOW)),
        77
    );
    assert_eq!(
        seccomp::sys_seccomp(
            SET_MODE_FILTER,
            FILTER_FLAG_TSYNC | FILTER_FLAG_TSYNC_ESRCH,
            filter(RET_ALLOW)
        ),
        -errno::ESRCH
    );
    assert_eq!(
        seccomp::sys_seccomp(SET_MODE_FILTER, 1 << 31, 0),
        -errno::EINVAL
    );
    assert_eq!(
        seccomp::sys_seccomp(GET_ACTION_AVAIL, FILTER_FLAG_TSYNC, 0),
        -errno::EINVAL
    );
    assert_eq!(task.seccomp_mode(), MODE_FILTER);
    INSTALL_FAILURE.set(None);
    assert_eq!(
        seccomp::sys_seccomp(SET_MODE_FILTER, FILTER_FLAG_TSYNC_ESRCH, filter(RET_ALLOW)),
        0
    );
    assert!(!LAST_SYNCHRONIZE.get());
}

#[test]
fn notification_sizes_are_fixed_codec_and_flag_combinations_precede_user_copy() {
    use syscall::seccomp::sys_seccomp;
    let _task = current(0);
    let destination = user(vec![0; 6]);
    assert_eq!(sys_seccomp(GET_NOTIF_SIZES, 0, destination), 0);
    USERS.with_borrow(|users| {
        assert_eq!(
            users[&destination],
            [
                80u16.to_ne_bytes(),
                24u16.to_ne_bytes(),
                64u16.to_ne_bytes()
            ]
            .concat()
        )
    });
    for flags in [
        FILTER_FLAG_TSYNC | FILTER_FLAG_NEW_LISTENER,
        FILTER_FLAG_WAIT_KILLABLE_RECV,
    ] {
        assert_eq!(sys_seccomp(SET_MODE_FILTER, flags, 0), -errno::EINVAL);
    }
    assert_eq!(
        sys_seccomp(GET_NOTIF_SIZES, FILTER_FLAG_LOG, destination),
        -errno::EINVAL
    );
}
