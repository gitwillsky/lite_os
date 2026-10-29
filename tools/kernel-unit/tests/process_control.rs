//! 直接执行生产 prctl codec，验证严格参数和不可撤销的 calling-Thread state。

use std::cell::Cell;

#[path = "../../../kernel/src/execution/task/model/address_space/dumpability.rs"]
mod dumpability;

mod syscall {
    pub(crate) mod errno {
        pub(crate) const EINVAL: isize = 22;
        pub(crate) const EFAULT: isize = 14;
    }
}

// 新 seccomp routes 在独立 seccomp_abi fixture 中执行真实 owner；这里仅保留旧 prctl codec。
mod seccomp {
    pub(crate) fn get_mode() -> isize {
        0
    }
    pub(crate) fn set_mode(_: usize, _: usize) -> isize {
        -22
    }
}

// Capability state/codec is exercised in capabilities + thread_credentials fixtures.
// These route-specific fixtures never invoke capability prctl.
mod capabilities {
    pub(crate) fn capability_error(_: ()) -> isize {
        unreachable!("capability route seam")
    }
}

#[path = "../../../kernel/src/syscall/process_control.rs"]
mod process_control;

#[test]
fn no_new_privileges_is_strict_irreversible_and_thread_scoped() {
    use process_control::sys_prctl;
    assert_eq!(sys_prctl(39, 0, [0; 3]), 0);
    assert_eq!(sys_prctl(0xabcd_ef01_0000_0027, 0, [0; 3]), 0);
    for argument in [0, 2, usize::MAX] {
        assert_eq!(sys_prctl(38, argument, [0; 3]), -22);
    }
    for index in 0..3 {
        let mut remaining = [0; 3];
        remaining[index] = 1;
        assert_eq!(sys_prctl(38, 1, remaining), -22);
        assert_eq!(sys_prctl(39, 0, remaining), -22);
    }
    assert_eq!(sys_prctl(39, 1, [0; 3]), -22);
    assert_eq!(sys_prctl(39, 0, [0; 3]), 0);
    assert_eq!(sys_prctl(0xabcd_ef01_0000_0026, 1, [0; 3]), 0);
    assert_eq!(sys_prctl(38, 1, [0; 3]), 0);
    assert_eq!(sys_prctl(38, 0, [0; 3]), -22);
    assert_eq!(sys_prctl(39, 0, [0; 3]), 1);
    std::thread::spawn(|| assert_eq!(sys_prctl(39, 0, [0; 3]), 0))
        .join()
        .unwrap();
    assert_eq!(sys_prctl(39, 0, [0; 3]), 1);
}

#[test]
fn parent_death_signal_codec_remains_available() {
    use process_control::sys_prctl;
    assert_eq!(sys_prctl(1, 9, [0; 3]), 0);
    assert_eq!(sys_prctl(2, 1, [0; 3]), 0);
    assert_eq!(crate::execution::task::copied_signal(), Some(9));
    assert_eq!(sys_prctl(2, 0, [0; 3]), -14);
    assert_eq!(sys_prctl(1, 65, [0; 3]), -22);
    assert_eq!(sys_prctl(usize::MAX, 0, [0; 3]), -22);
}

#[test]
fn dumpability_codec_accepts_only_zero_one_and_ignores_unused_arguments() {
    use process_control::sys_prctl;
    assert_eq!(sys_prctl(3, usize::MAX, [usize::MAX; 3]), 1);
    assert_eq!(sys_prctl(4, 0, [1, 2, 3]), 0);
    for argument in [2, 3, usize::MAX] {
        assert_eq!(sys_prctl(4, argument, [0; 3]), -22);
        assert_eq!(sys_prctl(3, 0, [0; 3]), 0);
    }
    assert_eq!(sys_prctl(0xabcd_ef01_0000_0004, 1, [0; 3]), 0);
    assert_eq!(sys_prctl(0xabcd_ef01_0000_0003, 0, [0; 3]), 1);
}

pub(crate) mod execution {
    pub(crate) mod task {
        use crate::Cell;
        use crate::dumpability::Dumpability;
        thread_local! {
            static NO_NEW_PRIVS: Cell<bool> = const { Cell::new(false) };
            static PARENT_DEATH_SIGNAL: Cell<usize> = const { Cell::new(0) };
            static COPYOUT: Cell<Option<i32>> = const { Cell::new(None) };
            // MM membership 是 codec fixture seam；共享/复制行为由真实 guest 验证。
            static MM_DUMPABILITY: Dumpability = Dumpability::new(true);
        }
        pub(crate) struct Task;
        pub(crate) fn current_task() -> Option<Task> {
            Some(Task)
        }
        pub(crate) fn parent_death_signal(value: Option<usize>) -> Result<usize, ()> {
            if value.is_some_and(|signal| signal > 64) {
                return Err(());
            }
            Ok(PARENT_DEATH_SIGNAL.with(|state| {
                if let Some(value) = value {
                    state.set(value);
                }
                state.get()
            }))
        }
        impl Task {
            // 本夹具只验证 NNP/PDEATHSIG/dumpability；comm owner 不由此 codec seam 伪造。
            pub(crate) fn copy_from_user(&self, _: usize, _: &mut [u8]) -> Result<(), ()> {
                unreachable!("comm copyin outside focused prctl fixture")
            }
            pub(crate) fn set_comm(&self, _: &[u8]) {
                unreachable!("comm mutation outside focused prctl fixture")
            }
            pub(crate) fn comm_buffer(&self) -> [u8; 16] {
                unreachable!("comm read outside focused prctl fixture")
            }

            pub(crate) fn user_dumpable(&self) -> bool {
                MM_DUMPABILITY.with(Dumpability::user_dumpable)
            }
            pub(crate) fn set_user_dumpable(&self, user: bool) {
                MM_DUMPABILITY.with(|mm| mm.set_user_dumpable(user));
            }
            pub(crate) fn capability_prctl(
                &self,
                _: u32,
                _: usize,
                _: [usize; 3],
            ) -> Result<usize, ()> {
                unreachable!("capability route seam")
            }
            pub(crate) fn enable_no_new_privileges(&self) {
                NO_NEW_PRIVS.set(true);
            }
            pub(crate) fn no_new_privileges(&self) -> bool {
                NO_NEW_PRIVS.get()
            }
            pub(crate) fn copy_to_user(&self, address: usize, bytes: &[u8]) -> Result<(), ()> {
                if address == 0 {
                    return Err(());
                }
                COPYOUT.set(Some(i32::from_ne_bytes(bytes.try_into().unwrap())));
                Ok(())
            }
        }
        pub(crate) fn copied_signal() -> Option<i32> {
            COPYOUT.get()
        }
    }
}
