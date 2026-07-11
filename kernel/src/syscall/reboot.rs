use crate::{
    syscall::errno,
    system::{self, ResetKind},
};

/// @description 验证 Linux reboot magic/command 并映射到 SBI whole-system reset。
///
/// @param magic 必须为 `LINUX_REBOOT_MAGIC1`。
/// @param magic2 接受 Linux 当前及历史兼容 magic2。
/// @param command CAD toggle、halt/poweroff 或 restart command。
/// @param argument `RESTART2` 的用户字符串；当前 platform 不支持 restart reason。
/// @return CAD toggle 返回零；reset 成功不返回；非法参数或 SBI 错误返回负 errno。
pub(crate) fn sys_reboot(magic: usize, magic2: usize, command: usize, argument: usize) -> isize {
    const MAGIC1: usize = 0xfee1_dead;
    const MAGIC2: [usize; 4] = [0x2812_1969, 0x0512_1996, 0x1604_1998, 0x2011_2000];
    const CAD_OFF: usize = 0;
    const CAD_ON: usize = 0x89ab_cdef;
    const RESTART: usize = 0x0123_4567;
    const RESTART2: usize = 0xa1b2_c3d4;
    const HALT: usize = 0xcdef_0123;
    const POWER_OFF: usize = 0x4321_fedc;
    if magic != MAGIC1 || !MAGIC2.contains(&magic2) {
        return -errno::EINVAL;
    }
    match command {
        CAD_OFF => {
            system::set_ctrl_alt_del(false);
            0
        }
        CAD_ON => {
            system::set_ctrl_alt_del(true);
            0
        }
        RESTART => reset(ResetKind::ColdReboot),
        RESTART2 if argument != 0 => -errno::EINVAL,
        HALT | POWER_OFF => reset(ResetKind::Shutdown),
        _ => -errno::EINVAL,
    }
}

fn reset(kind: ResetKind) -> isize {
    match system::reset(kind) {
        Ok(()) | Err(_) => -errno::EIO,
    }
}
