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
    // Linux reboot 的 magic 是 int、cmd 是 unsigned int；musl reboot(int) 会把
    // RB_HALT_SYSTEM 符号扩展到 XLEN。缺少截断会返回 EINVAL，BusyBox init 随后退出。
    let magic = magic as u32;
    let magic2 = magic2 as u32;
    let command = command as u32;
    const MAGIC1: u32 = 0xfee1_dead;
    const MAGIC2: [u32; 4] = [0x2812_1969, 0x0512_1996, 0x1604_1998, 0x2011_2000];
    const CAD_OFF: u32 = 0;
    const CAD_ON: u32 = 0x89ab_cdef;
    const RESTART: u32 = 0x0123_4567;
    const RESTART2: u32 = 0xa1b2_c3d4;
    const HALT: u32 = 0xcdef_0123;
    const POWER_OFF: u32 = 0x4321_fedc;
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
