//! `statx`：按 Linux `struct statx` 布局报告 inode metadata。

use super::{AT_SYMLINK_NOFOLLOW, copy_stat_path, errno, resolve_stat_path};
use crate::{execution::task::current_task, fs::DeviceNumber, fs::InodeMetadata};

const AT_NO_AUTOMOUNT: u32 = 0x800;
pub(in crate::syscall) const AT_EMPTY_PATH: u32 = 0x1000;
/// `AT_STATX_SYNC_TYPE` 的两个位同时置位是互斥的 `FORCE_SYNC|DONT_SYNC`。
const AT_STATX_SYNC_TYPE: u32 = 0x6000;

const STATX_TYPE: u32 = 0x001;
const STATX_MODE: u32 = 0x002;
const STATX_NLINK: u32 = 0x004;
const STATX_UID: u32 = 0x008;
const STATX_GID: u32 = 0x010;
const STATX_ATIME: u32 = 0x020;
const STATX_MTIME: u32 = 0x040;
const STATX_CTIME: u32 = 0x080;
const STATX_INO: u32 = 0x100;
const STATX_SIZE: u32 = 0x200;
const STATX_BLOCKS: u32 = 0x400;
const STATX_BASIC_STATS: u32 = 0x7ff;
const STATX_BTIME: u32 = 0x800;
const STATX_MNT_ID: u32 = 0x1000;
/// Linux 保留给内部使用；用户态传入即 EINVAL。
const STATX__RESERVED: u32 = 0x8000_0000;

const STATX_BYTES: usize = 256;

fn put(bytes: &mut [u8; STATX_BYTES], offset: usize, value: &[u8]) {
    bytes[offset..offset + value.len()].copy_from_slice(value);
}

fn put_timestamp(bytes: &mut [u8; STATX_BYTES], offset: usize, seconds: u64, nanoseconds: u32) {
    put(bytes, offset, &(seconds as i64).to_ne_bytes());
    put(bytes, offset + 8, &nanoseconds.to_ne_bytes());
}

/// 报告 pathname 或 fd 对应 inode 的扩展 metadata。
///
/// 无论请求的 mask 是什么，都返回当前能提供的全部字段，并在 `stx_mask` 里声明；`BTIME` 只在
/// filesystem 记录创建时间时出现（目前 ext4）。`stx_attributes_mask` 为 0：immutable/append/nodump
/// 等 inode 属性在本内核没有对应语义，不能声称支持。
///
/// # Parameters
///
/// - `fd` / `name`: `*at` 语义的目录 fd 与 pathname。
/// - `flags`: `AT_SYMLINK_NOFOLLOW`、`AT_EMPTY_PATH`、`AT_NO_AUTOMOUNT` 与互斥的同步类型位。
/// - `mask`: 请求的 `STATX_*`；保留位非法。
/// - `buffer`: 256 字节 `struct statx` 输出地址。
///
/// # Returns
///
/// 成功返回 0；非法 flags/mask 返回 `-EINVAL`，路径/权限错误与 `stat` 一致，输出不可写返回 `-EFAULT`。
pub(crate) fn sys_statx(fd: isize, name: *const u8, flags: u32, mask: u32, buffer: usize) -> isize {
    if !statx_request_valid(flags, mask) {
        return -errno::EINVAL;
    }
    let Some(task) = current_task() else {
        return -errno::ESRCH;
    };
    let path = match copy_stat_path(&task, name, flags & AT_EMPTY_PATH != 0) {
        Ok(path) => path,
        Err(error) => return error,
    };
    statx_at(&task, fd, path, flags, buffer)
}

/// `flags` 与 `mask` 是否都是 `statx` 接受的取值（未知 flags、互斥的同步类型位、保留 mask 位非法）。
pub(in crate::syscall) fn statx_request_valid(flags: u32, mask: u32) -> bool {
    flags & !(AT_SYMLINK_NOFOLLOW | AT_NO_AUTOMOUNT | AT_EMPTY_PATH | AT_STATX_SYNC_TYPE) == 0
        && flags & AT_STATX_SYNC_TYPE != AT_STATX_SYNC_TYPE
        && mask & STATX__RESERVED == 0
}

/// 在已复制的路径上执行 `statx` 并写回用户缓冲区；调用者已用 [`statx_request_valid`] 校验参数。
///
/// 显式接收 task，使 io_uring 的车道线程能以提交线程的 cwd、凭据与地址空间执行它。
///
/// # Returns
///
/// 成功返回 0；路径/权限错误与 `stat` 一致，输出不可写返回 `-EFAULT`。
pub(in crate::syscall) fn statx_at(
    task: &crate::execution::task::TaskControlBlock,
    fd: isize,
    path: alloc::vec::Vec<u8>,
    flags: u32,
    buffer: usize,
) -> isize {
    let (metadata, mount_id) = match resolve_stat_path(
        task,
        fd,
        path,
        flags & AT_SYMLINK_NOFOLLOW != 0,
        flags & AT_EMPTY_PATH != 0,
    ) {
        Ok(target) => target,
        Err(error) => return error,
    };
    let bytes = encode_statx(task, &metadata, mount_id);
    task.copy_to_user(buffer, &bytes)
        .map_or(-errno::EFAULT, |()| 0)
}

fn encode_statx(
    task: &crate::execution::task::TaskControlBlock,
    metadata: &InodeMetadata,
    mount_id: u64,
) -> [u8; STATX_BYTES] {
    let mut bytes = [0u8; STATX_BYTES];
    let mut mask = STATX_BASIC_STATS | STATX_MNT_ID;
    debug_assert_eq!(
        STATX_BASIC_STATS,
        STATX_TYPE
            | STATX_MODE
            | STATX_NLINK
            | STATX_UID
            | STATX_GID
            | STATX_ATIME
            | STATX_MTIME
            | STATX_CTIME
            | STATX_INO
            | STATX_SIZE
            | STATX_BLOCKS
    );
    let user_namespace = task.user_namespace();
    put(&mut bytes, 4, &metadata.block_size.to_ne_bytes());
    put(&mut bytes, 16, &metadata.links.to_ne_bytes());
    put(
        &mut bytes,
        20,
        &user_namespace.display_id(true, metadata.uid).to_ne_bytes(),
    );
    put(
        &mut bytes,
        24,
        &user_namespace.display_id(false, metadata.gid).to_ne_bytes(),
    );
    put(&mut bytes, 28, &(metadata.mode as u16).to_ne_bytes());
    put(&mut bytes, 32, &metadata.inode.to_ne_bytes());
    put(&mut bytes, 40, &metadata.size.to_ne_bytes());
    put(&mut bytes, 48, &metadata.blocks.to_ne_bytes());
    put_timestamp(
        &mut bytes,
        64,
        metadata.atime,
        metadata.fine_times.atime_nsec,
    );
    if let Some((seconds, nanoseconds)) = metadata.fine_times.birth {
        put_timestamp(&mut bytes, 80, seconds, nanoseconds);
        mask |= STATX_BTIME;
    }
    put_timestamp(
        &mut bytes,
        96,
        metadata.ctime,
        metadata.fine_times.ctime_nsec,
    );
    put_timestamp(
        &mut bytes,
        112,
        metadata.mtime,
        metadata.fine_times.mtime_nsec,
    );
    let rdev = metadata.device.unwrap_or(DeviceNumber::new(0, 0));
    put(&mut bytes, 128, &rdev.major.to_ne_bytes());
    put(&mut bytes, 132, &rdev.minor.to_ne_bytes());
    // st_dev 与 stat 同一编码，statx 把它拆成 major/minor。
    let device = DeviceNumber::decode(metadata.filesystem);
    put(&mut bytes, 136, &device.major.to_ne_bytes());
    put(&mut bytes, 140, &device.minor.to_ne_bytes());
    put(&mut bytes, 144, &mount_id.to_ne_bytes());
    put(&mut bytes, 0, &mask.to_ne_bytes());
    bytes
}
