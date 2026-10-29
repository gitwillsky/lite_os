//! 直接执行生产 security owner/BPF verifier，覆盖数据、权限状态、预算和 OOM 不发布。
#![feature(allocator_ext)]

extern crate alloc;
#[allow(dead_code, unused_imports)]
#[path = "../../../kernel/src/fallible_tree/mod.rs"]
mod fallible_tree;
#[allow(dead_code, unused_imports)]
#[path = "../src/namespace_fixture.rs"]
mod namespace;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use syscall_abi::seccomp::*;

#[allow(dead_code, unused_imports)]
#[path = "../src/seccomp_environment.rs"]
mod seccomp_environment;
use seccomp_environment::{file, fs, id, ipc};
macro_rules! info { ($($args:tt)*) => { let _ = format_args!($($args)*); }; }
mod arch {
    pub(crate) mod user {
        pub(crate) const AUDIT_ARCH: u32 = 0xc000_00b7;
    }
}

struct TaskControlBlock {
    security: seccomp::ThreadSecurityOwner,
}
impl TaskControlBlock {
    fn tid_identity(&self) -> alloc::sync::Arc<namespace::PidIdentity> {
        unreachable!("policy/codec fixture does not create notifications")
    }
    fn tid(&self) -> usize {
        1
    }
    fn thread(&self) -> &Self {
        self
    }
    fn user_program_counter(&self) -> usize {
        0x1234_5678_abcd_0004
    }
}

#[path = "seccomp/security.rs"]
mod model;

use model::seccomp;

use seccomp::{
    SeccompDecision as Decision, SeccompError as Error, ThreadSecurityOwner as Security,
};

impl Security {
    fn install_filter(&self, bytes: &[u8]) -> Result<(), Error> {
        let program = seccomp::ValidatedFilter::prepare(bytes, 0)?;
        let prepared = self.prepare_filter(&program)?;
        assert!(prepared.matches(self));
        drop(prepared.publish(self, false));
        Ok(())
    }
}

struct Allocator;
thread_local! {
    static FAIL_ALLOCATION: Cell<Option<usize>> = const { Cell::new(None) };
}
fn fail_allocation() -> bool {
    FAIL_ALLOCATION
        .try_with(|state| match state.get() {
            None => false,
            Some(0) => {
                state.set(None);
                true
            }
            Some(count) => {
                state.set(Some(count - 1));
                false
            }
        })
        .unwrap_or(false)
}
// SAFETY: 成功 allocation/deallocation 委托 System，故障只返回 NULL 不伪造 allocation。
unsafe impl GlobalAlloc for Allocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if fail_allocation() {
            core::ptr::null_mut()
        } else {
            unsafe { System.alloc(layout) }
        }
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) }
    }
    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        if fail_allocation() {
            core::ptr::null_mut()
        } else {
            unsafe { System.realloc(pointer, layout, size) }
        }
    }
}
#[global_allocator]
static ALLOCATOR: Allocator = Allocator;

fn program(instructions: &[(u16, u8, u8, u32)]) -> Vec<u8> {
    instructions
        .iter()
        .flat_map(|&(code, yes, no, value)| {
            let mut bytes = [0; 8];
            bytes[..2].copy_from_slice(&code.to_ne_bytes());
            bytes[2] = yes;
            bytes[3] = no;
            bytes[4..].copy_from_slice(&value.to_ne_bytes());
            bytes
        })
        .collect()
}
fn constant(action: u32) -> Vec<u8> {
    program(&[(0x06, 0, 0, action)])
}
fn evaluate(security: &Security) -> Decision {
    security.evaluate(172, [0; 6], arch::user::AUDIT_ARCH, || 0x1234, |_| ())
}

#[test]
fn disabled_fast_path_does_not_read_context_or_lock_security() {
    let security = Security::default();
    assert_eq!(
        security.evaluate(
            usize::MAX,
            [usize::MAX; 6],
            0,
            || panic!("disabled read PC"),
            |_| ()
        ),
        Decision::Allow
    );
    assert_eq!(security.mode(), MODE_DISABLED);
    security.enable_no_new_privileges();
    let child = security.clone();
    assert!(child.no_new_privileges());
    assert!(!Security::default().no_new_privileges());
}

#[test]
fn strict_is_irreversible_and_only_allows_linux_mode_one_calls() {
    let task = TaskControlBlock {
        security: Security::default(),
    };
    task.security.enable_strict().unwrap();
    assert_eq!(task.seccomp_mode(), MODE_STRICT);
    for number in [63, 64, 93, 139] {
        assert_eq!(task.evaluate_seccomp(number, [0; 6]), Decision::Allow);
    }
    for number in [94, 172, 277, 0, usize::MAX] {
        assert_eq!(
            task.evaluate_seccomp(number, [0; 6]),
            Decision::KillThread(9)
        );
    }
    assert_eq!(
        task.security.install_filter(&constant(RET_ALLOW)),
        Err(Error::Invalid)
    );
    assert_eq!(task.security.clone().mode(), MODE_STRICT);
}

#[test]
fn signed_precedence_and_newest_data_cannot_relax_an_ancestor() {
    let security = Security::default();
    security.install_filter(&constant(RET_ERRNO | 1)).unwrap();
    let child = security.clone();
    security.install_filter(&constant(RET_ERRNO | 13)).unwrap();
    security.install_filter(&constant(RET_ALLOW)).unwrap();
    assert_eq!(evaluate(&security), Decision::Errno(13));
    assert_eq!(evaluate(&child), Decision::Errno(1));
    security
        .install_filter(&constant(RET_KILL_PROCESS))
        .unwrap();
    assert_eq!(evaluate(&security), Decision::KillProcess);
    assert_eq!(security.enable_strict(), Err(Error::Invalid));
    let unknown = Security::default();
    unknown.install_filter(&constant(0x6000_0000)).unwrap();
    unknown.install_filter(&constant(RET_ERRNO | 2)).unwrap();
    assert_eq!(evaluate(&unknown), Decision::Errno(2));
}

#[test]
fn action_decoding_caps_errno_and_preserves_trap_data() {
    for (action, expected) in [
        (RET_KILL_THREAD, Decision::KillThread(31)),
        (RET_TRAP | 42, Decision::Trap(42)),
        (RET_ERRNO | 0xffff, Decision::Errno(4095)),
        (RET_ERRNO, Decision::Errno(0)),
        (RET_TRACE, Decision::NoObserver),
        (RET_USER_NOTIF, Decision::NoObserver),
        (RET_LOG, Decision::Log),
        (RET_ALLOW, Decision::Allow),
        (0xffff_0000, Decision::KillProcess),
    ] {
        let security = Security::default();
        security.install_filter(&constant(action)).unwrap();
        assert_eq!(evaluate(&security), expected);
    }
}

#[test]
fn all_seccomp_data_words_have_the_standard_lp64_layout() {
    let number = 0xdead_beef_8000_1234usize;
    let args = [0x1234_5678_9000_0001, 2, 3, 4, 5, 0xfedc_ba98_7654_3210];
    let pc = 0xabcd_0123_4455_6677usize;
    let expected = [
        number as u32,
        arch::user::AUDIT_ARCH,
        pc as u32,
        (pc >> 32) as u32,
        args[0] as u32,
        (args[0] >> 32) as u32,
        2,
        0,
        3,
        0,
        4,
        0,
        5,
        0,
        args[5] as u32,
        (args[5] >> 32) as u32,
    ];
    for (word, value) in expected.iter().enumerate() {
        let security = Security::default();
        let bytes = program(&[
            (0x20, 0, 0, word as u32 * 4),
            (0x15, 0, 1, *value),
            (0x06, 0, 0, RET_ALLOW),
            (0x06, 0, 0, RET_KILL_PROCESS),
        ]);
        security.install_filter(&bytes).unwrap();
        assert_eq!(
            security.evaluate(number, args, arch::user::AUDIT_ARCH, || pc, |_| ()),
            Decision::Allow
        );
    }
}

#[test]
fn verifier_rejects_unsafe_programs_without_publishing() {
    let security = Security::default();
    for bytes in [
        vec![],
        vec![0; 7],
        program(&[(0x00, 0, 0, 1)]),
        constant(0).repeat(4097),
        program(&[(0x05, 0, 0, u32::MAX), (0x06, 0, 0, RET_ALLOW)]),
        program(&[(0x15, 255, 0, 1), (0x06, 0, 0, RET_ALLOW)]),
        program(&[(0x20, 0, 0, 64), (0x06, 0, 0, RET_ALLOW)]),
        program(&[(0x20, 0, 0, 1), (0x06, 0, 0, RET_ALLOW)]),
        program(&[(0x21, 0, 0, 0), (0x06, 0, 0, RET_ALLOW)]),
        program(&[(0x02, 0, 0, 16), (0x06, 0, 0, RET_ALLOW)]),
        program(&[(0x60, 0, 0, 0), (0x06, 0, 0, RET_ALLOW)]),
        program(&[(0x34, 0, 0, 0), (0x06, 0, 0, RET_ALLOW)]),
        program(&[(0x64, 0, 0, 32), (0x06, 0, 0, RET_ALLOW)]),
        program(&[(0x94, 0, 0, 1), (0x06, 0, 0, RET_ALLOW)]),
        // one incoming branch skips the scratch store
        program(&[
            (0x15, 1, 0, 0),
            (0x02, 0, 0, 0),
            (0x60, 0, 0, 0),
            (0x06, 0, 0, RET_ALLOW),
        ]),
    ] {
        assert_eq!(security.install_filter(&bytes), Err(Error::Invalid));
        assert_eq!(security.mode(), MODE_DISABLED);
    }
}

#[test]
fn alu_scratch_and_branch_execution_is_32_bit_wrapping() {
    let cases: &[(u16, u32, u32, u32)] = &[
        (0x04, u32::MAX, 1, 0),
        (0x14, 0, 1, u32::MAX),
        (0x24, u32::MAX, 2, u32::MAX - 1),
        (0x34, 100, 7, 14),
        (0x44, 0x100, 3, 0x103),
        (0x54, 0x123, 0xf, 3),
        (0x64, 1, 31, 0x8000_0000),
        (0x74, 0x8000_0000, 31, 1),
        (0xa4, 3, 1, 2),
    ];
    for &(opcode, left, right, expected) in cases {
        for via_index in [false, true] {
            let security = Security::default();
            let bytes = program(&[
                (0x00, 0, 0, left),
                (0x01, 0, 0, right),
                (opcode | if via_index { 8 } else { 0 }, 0, 0, right),
                (0x02, 0, 0, 0),
                (0x00, 0, 0, 0),
                (0x60, 0, 0, 0),
                (0x15, 0, 1, expected),
                (0x06, 0, 0, RET_ALLOW),
                (0x06, 0, 0, RET_KILL_PROCESS),
            ]);
            security.install_filter(&bytes).unwrap();
            assert_eq!(evaluate(&security), Decision::Allow);
        }
    }
    let zero = Security::default();
    zero.install_filter(&program(&[
        (0x01, 0, 0, 0),
        (0x3c, 0, 0, 0),
        (0x06, 0, 0, RET_ALLOW),
    ]))
    .unwrap();
    assert_eq!(evaluate(&zero), Decision::KillThread(31));
    let masked = Security::default();
    masked
        .install_filter(&program(&[
            (0x00, 0, 0, RET_ALLOW),
            (0x01, 0, 0, 32),
            (0x6c, 0, 0, 0),
            (0x16, 0, 0, 0),
        ]))
        .unwrap();
    assert_eq!(evaluate(&masked), Decision::Allow);
}

#[test]
fn every_install_allocation_failure_keeps_the_old_filter() {
    let old = constant(RET_ERRNO | 13);
    let new = constant(RET_KILL_PROCESS);
    for failure in 0..5 {
        let security = Security::default();
        security.install_filter(&old).unwrap();
        FAIL_ALLOCATION.set(Some(failure));
        let result = security.install_filter(&new);
        FAIL_ALLOCATION.set(None);
        assert_eq!(result, Err(Error::OutOfMemory), "allocation {failure}");
        assert_eq!(security.mode(), MODE_FILTER);
        assert_eq!(evaluate(&security), Decision::Errno(13));
    }
}

#[test]
fn lengths_transfers_negation_and_index_scratch_use_classic_semantics() {
    let security = Security::default();
    security
        .install_filter(&program(&[
            (0x80, 0, 0, 0),  // A = sizeof(seccomp_data)
            (0x07, 0, 0, 0),  // X = A
            (0x03, 0, 0, 15), // scratch[15] = X
            (0x01, 0, 0, 0),
            (0x61, 0, 0, 15),
            (0x87, 0, 0, 0), // X reload, A = X
            (0x84, 0, 0, 0), // A = -64
            (0x81, 0, 0, 0),
            (0x0c, 0, 0, 0), // X = 64, A wraps to 0
            (0x15, 1, 0, 0),
            (0x06, 0, 0, RET_KILL_PROCESS),
            (0x06, 0, 0, RET_ALLOW),
        ]))
        .unwrap();
    assert_eq!(evaluate(&security), Decision::Allow);
    for (opcode, yes, no, operand) in [
        (0x15, 5, 4, 5),
        (0x25, 6, 5, 5),
        (0x35, 5, 4, 5),
        (0x45, 5, 4, 1),
    ] {
        for via_index in [false, true] {
            let security = Security::default();
            let bytes = program(&[
                (0x20, 0, 0, 16),
                (0x01, 0, 0, operand),
                (opcode | if via_index { 8 } else { 0 }, 1, 0, operand),
                (0x06, 0, 0, RET_ERRNO | 13),
                (0x06, 0, 0, RET_ALLOW),
            ]);
            security.install_filter(&bytes).unwrap();
            assert_eq!(
                security.evaluate(172, [yes; 6], 0, || 0, |_| ()),
                Decision::Allow
            );
            assert_eq!(
                security.evaluate(172, [no; 6], 0, || 0, |_| ()),
                Decision::Errno(13)
            );
        }
    }
}

#[test]
fn maximum_filter_set_drop_is_bounded_on_a_small_stack() {
    std::thread::Builder::new()
        .stack_size(64 * 1024)
        .spawn(|| {
            let security = Security::default();
            let bytes = constant(RET_ALLOW);
            for _ in 0..6554 {
                security.install_filter(&bytes).unwrap();
            }
            assert_eq!(security.install_filter(&bytes), Err(Error::OutOfMemory));
            let child = security.clone();
            drop(security);
            assert_eq!(evaluate(&child), Decision::Allow);
            drop(child);
        })
        .unwrap()
        .join()
        .unwrap();
}

#[path = "."]
pub(crate) mod execution {
    #[allow(dead_code, unused_imports)]
    #[path = "../src/sync.rs"]
    pub(crate) mod sync;
    pub(crate) mod task {
        pub(crate) fn current_task() -> Option<alloc::sync::Arc<crate::TaskControlBlock>> {
            None
        }
    }
}
