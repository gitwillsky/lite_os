use crate::{
    syscall::errno,
    system::{self, ResetKind},
};

/// 验证 Linux reboot magic/command 并映射到 platform whole-system reset。
///
/// # Parameters
///
/// - `magic`: 必须为 `LINUX_REBOOT_MAGIC1`。
/// - `magic2`: 接受 Linux 当前及历史兼容 magic2。
/// - `command`: CAD toggle、halt/poweroff 或 restart command。
/// - `argument`: `RESTART2` 的用户字符串；当前 platform 不支持 restart reason。
///
/// # Returns
///
/// reset 成功不返回；CAD 未支持返回 EOPNOTSUPP；非法参数或 platform 错误返回负 errno。
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
        // input 尚无 CAD consumer；拒绝命令，避免发布永远不会被消费的策略。
        CAD_OFF | CAD_ON => -errno::EOPNOTSUPP,
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
