//! 块设备节点的 `BLK*` ioctl（Linux `block/ioctl.c` 中 mkfs/blkid/lsblk/fdisk 实际使用的子集）。

use alloc::sync::Arc;
use syscall_abi::errno;

use super::{
    BlockNode, FileSystemError,
    device::{DeviceError, IoctlCall, UserFault},
};

const BLKROGET: usize = 0x125e;
const BLKRRPART: usize = 0x125f;
const BLKGETSIZE: usize = 0x1260;
const BLKFLSBUF: usize = 0x1261;
const BLKSSZGET: usize = 0x1268;
const BLKBSZGET: usize = 0x8008_1270;
const BLKGETSIZE64: usize = 0x8008_1272;
const BLKIOMIN: usize = 0x1278;
const BLKIOOPT: usize = 0x1279;
const BLKALIGNOFF: usize = 0x127a;
const BLKPBSZGET: usize = 0x127b;

/// 设备地址空间以 512 字节扇区计，与块大小无关（`BLKGETSIZE` 的单位）。
const SECTOR: u64 = 512;

fn fault(_: UserFault) -> DeviceError {
    DeviceError::Errno(errno::EFAULT)
}

fn put(call: &IoctlCall<'_>, bytes: &[u8]) -> Result<isize, DeviceError> {
    call.user.write(call.argument, bytes).map_err(fault)?;
    Ok(0)
}

/// 执行一个 `BLK*` ioctl。
///
/// # Errors
///
/// 不认识的请求返回 `ENOTTY`；用户地址不可写返回 `EFAULT`；`BLKFLSBUF` 需要特权返回 `EACCES`。
pub(super) fn ioctl(block: &Arc<BlockNode>, call: &IoctlCall<'_>) -> Result<isize, DeviceError> {
    let block_size = block.device().block_size() as u32;
    match call.request {
        BLKGETSIZE64 => put(call, &block.capacity().to_ne_bytes()),
        BLKGETSIZE => {
            let sectors = block.capacity() / SECTOR;
            put(call, &sectors.to_ne_bytes())
        }
        BLKSSZGET | BLKPBSZGET => put(call, &block_size.to_ne_bytes()),
        BLKBSZGET => put(call, &(block_size as i32).to_ne_bytes()),
        BLKIOMIN => put(call, &block_size.to_ne_bytes()),
        // 没有 RAID/条带：最优 I/O 大小未知（0），对齐偏移为 0。
        BLKIOOPT | BLKALIGNOFF => put(call, &0u32.to_ne_bytes()),
        BLKROGET => put(call, &0i32.to_ne_bytes()),
        // 没有分区表支持：整盘就是唯一的设备，重读分区表恒为空操作。
        BLKRRPART => Ok(0),
        BLKFLSBUF => {
            if !call.privileged {
                return Err(DeviceError::Errno(errno::EACCES));
            }
            // 缓冲页写回 + 设备 flush；挂载中的设备没有缓冲页，只做设备 flush。
            super::page_cache::writeback_cached(block.cache_id())
                .and_then(|()| block.flush())
                .map_err(|error| {
                    DeviceError::Errno(match error {
                        FileSystemError::OutOfMemory => errno::ENOMEM,
                        FileSystemError::NoSpace => errno::ENOSPC,
                        _ => errno::EIO,
                    })
                })?;
            Ok(0)
        }
        _ => Err(DeviceError::Errno(errno::ENOTTY)),
    }
}
