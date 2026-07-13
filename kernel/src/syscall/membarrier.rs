use crate::task::{register_private_memory_barrier, synchronize_private_memory};

use super::errno;

const MEMBARRIER_CMD_QUERY: usize = 0;
const MEMBARRIER_CMD_PRIVATE_EXPEDITED: usize = 1 << 3;
const MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED: usize = 1 << 4;
const SUPPORTED_COMMANDS: usize =
    MEMBARRIER_CMD_PRIVATE_EXPEDITED | MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED;

/// @description 执行 Linux membarrier command 的已声明子集。
///
/// @param command Linux UAPI `enum membarrier_cmd` 单个 command bit 或 QUERY。
/// @param flags 当前已声明 command 仅接受零。
/// @param cpu_id flags 不含 CPU selector 时按 Linux 语义忽略。
/// @return QUERY 返回 command mask，注册/屏障成功返回 0，失败返回负 errno。
/// @errors 非零 flags/未知 command 返回 EINVAL；未注册执行 private expedited 返回 EPERM。
pub(super) fn sys_membarrier(command: usize, flags: usize, _cpu_id: usize) -> isize {
    if flags != 0 {
        return -errno::EINVAL;
    }
    match command {
        MEMBARRIER_CMD_QUERY => SUPPORTED_COMMANDS as isize,
        MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED => {
            register_private_memory_barrier();
            0
        }
        MEMBARRIER_CMD_PRIVATE_EXPEDITED => {
            if synchronize_private_memory() {
                0
            } else {
                -errno::EPERM
            }
        }
        _ => -errno::EINVAL,
    }
}
