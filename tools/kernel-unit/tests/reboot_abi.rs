#![allow(dead_code)]

use std::cell::Cell;

thread_local! {
    static EFFECTIVE_UID: Cell<u32> = const { Cell::new(0) };
    static RESET: Cell<Option<system::ResetKind>> = const { Cell::new(None) };
}

mod task {
    pub(crate) struct Task;

    pub(crate) fn current_task() -> Option<Task> {
        Some(Task)
    }

    impl Task {
        pub(crate) fn has_initial_capability(
            &self,
            cap: syscall_abi::capability::Capability,
        ) -> bool {
            assert_eq!(cap, syscall_abi::capability::Capability::SYS_BOOT);
            super::EFFECTIVE_UID.get() == 0 // injected authorization, not production UID policy
        }
    }
}

#[path = "../../../syscall-abi/src/errno.rs"]
mod errno;

mod syscall {
    pub(crate) use crate::errno;
}

mod system {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum ResetKind {
        ColdReboot,
        Shutdown,
    }

    // 不执行 host reset；EIO 证明 production dispatcher 确实到达 platform seam。
    pub(crate) fn reset(kind: ResetKind) -> Result<(), ()> {
        super::RESET.set(Some(kind));
        Err(())
    }
}

#[path = "../../../kernel/src/syscall/reboot.rs"]
mod reboot;

#[test]
fn unprivileged_commands_never_reach_firmware() {
    EFFECTIVE_UID.set(1000);
    for command in [
        0,
        0x89ab_cdef,
        0x0123_4567,
        0xa1b2_c3d4,
        0xcdef_0123,
        0x4321_fedc,
    ] {
        RESET.set(None);
        assert_eq!(
            reboot::sys_reboot(0xfee1_dead, 0x2812_1969, command, 0),
            -errno::EPERM
        );
        assert_eq!(RESET.get(), None);
    }
    assert_eq!(
        reboot::sys_reboot(0, 0, usize::MAX, usize::MAX),
        -errno::EPERM
    );
    assert_eq!(RESET.get(), None);
    EFFECTIVE_UID.set(0);
}

#[test]
fn privileged_commands_select_the_declared_reset_kind() {
    for (command, kind) in [
        (0x0123_4567, system::ResetKind::ColdReboot),
        (0xcdef_0123, system::ResetKind::Shutdown),
        (0x4321_fedc, system::ResetKind::Shutdown),
    ] {
        RESET.set(None);
        assert_eq!(
            reboot::sys_reboot(0xfee1_dead, 0x2812_1969, command, 0),
            -errno::EIO
        );
        assert_eq!(RESET.get(), Some(kind));
    }
}

#[test]
fn reboot_int_arguments_accept_zero_and_sign_extended_registers() {
    for magic in [0xfee1_deadusize, 0xffff_ffff_fee1_dead] {
        for command in [0xcdef_0123usize, 0xffff_ffff_cdef_0123] {
            assert_eq!(
                reboot::sys_reboot(magic, 0x2812_1969, command, 0),
                -syscall::errno::EIO
            );
        }
    }
    assert_eq!(
        reboot::sys_reboot(
            0x1234_5678_fee1_dead,
            0xabcd_ef01_2812_1969,
            0x1234_5678_cdef_0123,
            0
        ),
        -syscall::errno::EIO
    );
}

#[test]
fn normalization_preserves_invalid_and_unsupported_command_errors() {
    assert_eq!(
        reboot::sys_reboot(0xfee1_dead, 0x2812_1969, 0xffff_ffff_89ab_cdef, 0),
        -syscall::errno::EOPNOTSUPP
    );
    assert_eq!(
        reboot::sys_reboot(0, 0x2812_1969, 0xcdef_0123, 0),
        -syscall::errno::EINVAL
    );
    assert_eq!(
        reboot::sys_reboot(0xfee1_dead, 0, 0xcdef_0123, 0),
        -syscall::errno::EINVAL
    );
    assert_eq!(
        reboot::sys_reboot(0xfee1_dead, 0x2812_1969, 0x7654_3210, 0),
        -syscall::errno::EINVAL
    );
    assert_eq!(
        reboot::sys_reboot(0xfee1_dead, 0x2812_1969, 0xa1b2_c3d4, 0x1_0000_0000),
        -syscall::errno::EINVAL
    );
}
