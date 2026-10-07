#![allow(dead_code)]

#[path = "../../../syscall-abi/src/errno.rs"]
mod errno;

mod syscall {
    pub(crate) use crate::errno;
}

mod system {
    #[derive(Clone, Copy)]
    pub(crate) enum ResetKind {
        ColdReboot,
        Shutdown,
    }

    // 不执行 host reset；EIO 证明 production dispatcher 确实到达 platform seam。
    pub(crate) fn reset(_kind: ResetKind) -> Result<(), ()> {
        Err(())
    }
}

#[path = "../../../kernel/src/syscall/reboot.rs"]
mod reboot;

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
