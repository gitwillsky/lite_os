use alloc::{sync::Arc, vec::Vec};
use core::mem;

mod access;
mod attributes;
mod fcntl;
mod flock;
mod io;
mod links;
mod namespace;
mod open;
mod pathname;
mod readlink;
pub(crate) mod statistics;
pub(crate) use access::sys_faccessat;
pub(crate) use attributes::{sys_fchmod, sys_fchmodat, sys_fchown, sys_fchownat};
pub(crate) use fcntl::sys_fcntl;
pub(crate) use flock::sys_flock;
pub(crate) use io::{
    sys_pread64, sys_preadv, sys_preadv2, sys_pwrite64, sys_pwritev, sys_pwritev2, sys_read,
    sys_readv, sys_write, sys_writev,
};
pub(crate) use links::{sys_linkat, sys_symlinkat};
pub(crate) use namespace::{sys_mkdirat, sys_mknodat, sys_renameat2, sys_unlinkat};
pub(crate) use open::{sys_chdir, sys_openat};
use pathname::{base, ferr, path};
pub(crate) use readlink::sys_readlinkat;

use crate::{
    fs::{
        CharacterDevice, DeviceKind, InodeMetadata, InodeType, O_ACCMODE, O_APPEND, O_CLOEXEC,
        O_NONBLOCK, O_RDONLY, O_WRONLY, OpenFileDescription, OpenFileKind, RegularFile,
        RegularFileWrite, TerminalAccess, TerminalRead, vfs,
    },
    ipc::{PIPE_BUF, Pipe, PipeDirection, PipeRead, PipeWaitCondition, PipeWrite},
    syscall::errno,
    task::{
        TaskControlBlock, WaitResult, create_pipe_endpoints, current_task, drain_terminal_input,
        send_kernel_thread_signal, send_thread_signal, wait_for_pipe,
    },
};

use super::tty::guard_terminal_access;
const AT_FDCWD: isize = -100;
const AT_SYMLINK_NOFOLLOW: u32 = 0x100;
pub(crate) fn sys_close(fd: usize) -> isize {
    current_task().map_or(-errno::ESRCH, |t| {
        t.fd_close(fd).map_or(-errno::EBADF, |_| 0)
    })
}

/// @description 创建一对共享 anonymous pipe 的 Linux read/write descriptors。
///
/// @param descriptors 用户态 `int[2]` 输出地址。
/// @param flags 只接受 `O_CLOEXEC|O_NONBLOCK`。
/// @return 成功返回零；参数、fd 容量、内存或 copyout 错误返回负 errno。
pub(crate) fn sys_pipe2(descriptors: usize, flags: u32) -> isize {
    if flags & !(O_CLOEXEC | O_NONBLOCK) != 0 {
        return -errno::EINVAL;
    }
    let task = current_task().expect("pipe2 requires current task");
    let (reader, writer) = match create_pipe_endpoints() {
        Ok(pair) => pair,
        Err(()) => return -errno::ENOMEM,
    };
    let pipe_flags = flags & O_NONBLOCK;
    let (read_fd, write_fd) = match task.fd_allocate_pair(
        OpenFileDescription::pipe(reader, O_RDONLY | pipe_flags),
        OpenFileDescription::pipe(writer, O_WRONLY | pipe_flags),
        flags & O_CLOEXEC != 0,
    ) {
        Ok(pair) => pair,
        Err(()) => return -errno::EMFILE,
    };
    let mut output = [0u8; 8];
    output[..4].copy_from_slice(&(read_fd as i32).to_ne_bytes());
    output[4..].copy_from_slice(&(write_fd as i32).to_ne_bytes());
    if task.copy_to_user(descriptors, &output).is_err() {
        task.fd_close(read_fd)
            .expect("new pipe read fd disappeared");
        task.fd_close(write_fd)
            .expect("new pipe write fd disappeared");
        return -errno::EFAULT;
    }
    0
}

pub(crate) fn sys_lseek(fd: usize, offset: i64, whence: u32) -> isize {
    let Some(task) = current_task() else {
        return -errno::ESRCH;
    };
    let Some(ofd) = task.fd_get(fd) else {
        return -errno::EBADF;
    };
    let Some(inode) = ofd.inode_ref() else {
        return -errno::ESPIPE;
    };
    let base = match whence {
        0 => 0,
        1 => *ofd.offset.lock() as i128,
        2 => inode.size() as i128,
        _ => return -errno::EINVAL,
    };
    let value = base + offset as i128;
    if value < 0 || value > u64::MAX as i128 {
        return -errno::EINVAL;
    }
    *ofd.offset.lock() = value as u64;
    value as isize
}

pub(crate) fn sys_ftruncate(fd: usize, size: u64) -> isize {
    let Some(task) = current_task() else {
        return -errno::ESRCH;
    };
    let Some(ofd) = task.fd_get(fd) else {
        return -errno::EBADF;
    };
    if *ofd.flags.lock() & O_ACCMODE == O_RDONLY {
        return -errno::EBADF;
    }
    if size > task.file_size_limit() {
        send_kernel_thread_signal(task.tgid(), task.tid(), 25)
            .expect("current ftruncate caller must exist");
        return -errno::EFBIG;
    }
    ofd.inode_ref()
        .ok_or(-errno::EINVAL)
        .and_then(|i| crate::fs::truncate(i, size).map_err(ferr))
        .map_or_else(|e| e, |_| 0)
}

/// @description 实现 Linux fallocate mode=0 的 regular-file space reservation。
/// @param fd 必须以 write access 打开的 regular-file descriptor。
/// @param mode 当前只接受零；其他 Linux allocation mode 明确返回 EOPNOTSUPP。
/// @param offset 非负 byte range 起点。
/// @param length 正数 byte range 长度。
/// @return 成功返回零；fd、range、RLIMIT_FSIZE、空间或 I/O 错误返回负 errno。
pub(crate) fn sys_fallocate(fd: usize, mode: usize, offset: i64, length: i64) -> isize {
    if mode != 0 {
        return -errno::EOPNOTSUPP;
    }
    if offset < 0 || length <= 0 {
        return -errno::EINVAL;
    }
    let Some(end) = offset
        .checked_add(length)
        .and_then(|value| u64::try_from(value).ok())
    else {
        return -errno::EFBIG;
    };
    let Some(task) = current_task() else {
        return -errno::ESRCH;
    };
    let Some(ofd) = task.fd_get(fd) else {
        return -errno::EBADF;
    };
    if *ofd.flags.lock() & O_ACCMODE == O_RDONLY {
        return -errno::EBADF;
    }
    let Some(inode) = ofd.inode_ref() else {
        return -errno::ENODEV;
    };
    if inode.inode_type() == InodeType::Directory {
        return -errno::EISDIR;
    }
    if inode.inode_type() != InodeType::File {
        return -errno::ENODEV;
    }
    if end > task.file_size_limit() {
        send_kernel_thread_signal(task.tgid(), task.tid(), 25)
            .expect("current fallocate caller must exist");
        return -errno::EFBIG;
    }
    crate::fs::allocate(inode, offset as u64, length as u64).map_or_else(ferr, |_| 0)
}

pub(super) fn sync_file(fd: usize) -> isize {
    let Some(task) = current_task() else {
        return -errno::ESRCH;
    };
    let Some(ofd) = task.fd_get(fd) else {
        return -errno::EBADF;
    };
    ofd.inode_ref().map_or(-errno::EINVAL, |i| {
        crate::fs::sync_inode(i).map_or_else(ferr, |_| 0)
    })
}

/// @description 把一个 inode-backed OFD 的数据与 metadata 提交到 stable storage。
///
/// @param fd 要同步的 descriptor。
/// @return 成功返回零；非 inode fd 或底层 I/O 失败返回负 errno。
pub(crate) fn sys_fsync(fd: usize) -> isize {
    sync_file(fd)
}

/// @description 提交文件数据及恢复该数据所需 metadata；当前同步 journal 模型与 fsync 共用提交边界。
///
/// @param fd 要同步的 descriptor。
/// @return 成功返回零；非 inode fd 或底层 I/O 失败返回负 errno。
pub(crate) fn sys_fdatasync(fd: usize) -> isize {
    sync_file(fd)
}

/// @description 将唯一 mounted filesystem 的已提交写入同步到 stable storage。
///
/// @return 按 Linux sync ABI 固定返回零；单个 writeback error 不通过该入口报告。
pub(crate) fn sys_sync() -> isize {
    let _ = vfs().sync();
    0
}

#[repr(C)]
struct UserStat {
    st_dev: u64,
    st_ino: u64,
    st_mode: u32,
    st_nlink: u32,
    st_uid: u32,
    st_gid: u32,
    st_rdev: u64,
    pad1: u64,
    st_size: i64,
    st_blksize: i32,
    pad2: i32,
    st_blocks: i64,
    st_atime: i64,
    st_atime_nsec: u64,
    st_mtime: i64,
    st_mtime_nsec: u64,
    st_ctime: i64,
    st_ctime_nsec: u64,
    unused: [u32; 2],
}

const _: () = assert!(mem::size_of::<UserStat>() == 128);

fn copy_stat(
    task: &TaskControlBlock,
    pointer: *mut u8,
    metadata: Option<InodeMetadata>,
    anonymous_mode: u32,
    anonymous_inode: u64,
) -> isize {
    let stat = if let Some(metadata) = metadata {
        UserStat {
            st_dev: metadata.filesystem,
            st_ino: metadata.inode,
            st_mode: metadata.mode,
            st_nlink: metadata.links,
            st_uid: metadata.uid,
            st_gid: metadata.gid,
            st_rdev: metadata.device.map_or(0, encode_device),
            pad1: 0,
            st_size: metadata.size as i64,
            st_blksize: metadata.block_size as i32,
            pad2: 0,
            st_blocks: metadata.blocks as i64,
            st_atime: metadata.atime as i64,
            st_atime_nsec: 0,
            st_mtime: metadata.mtime as i64,
            st_mtime_nsec: 0,
            st_ctime: metadata.ctime as i64,
            st_ctime_nsec: 0,
            unused: [0; 2],
        }
    } else {
        UserStat {
            st_dev: 0,
            st_ino: anonymous_inode,
            st_mode: anonymous_mode,
            st_nlink: 1,
            st_uid: 0,
            st_gid: 0,
            st_rdev: 0,
            pad1: 0,
            st_size: 0,
            st_blksize: 1,
            pad2: 0,
            st_blocks: 0,
            st_atime: 0,
            st_atime_nsec: 0,
            st_mtime: 0,
            st_mtime_nsec: 0,
            st_ctime: 0,
            st_ctime_nsec: 0,
            unused: [0; 2],
        }
    };
    // SAFETY: `UserStat` 是固定的 Linux/asm-generic C ABI POD，且切片不逃逸本函数。
    let bytes = unsafe {
        core::slice::from_raw_parts(
            (&stat as *const UserStat).cast::<u8>(),
            mem::size_of::<UserStat>(),
        )
    };
    task.copy_to_user(pointer as usize, bytes)
        .map_or(-errno::EFAULT, |_| 0)
}

fn encode_device(device: DeviceKind) -> u64 {
    let (major, minor) = device.numbers();
    u64::from(minor & 0xff)
        | (u64::from(major & 0xfff) << 8)
        | (u64::from(minor & !0xff) << 12)
        | (u64::from(major & !0xfff) << 32)
}

fn character_metadata(device: DeviceKind) -> InodeMetadata {
    InodeMetadata {
        filesystem: 2,
        inode: device.inode(),
        kind: InodeType::CharacterDevice,
        mode: device.mode(),
        links: 1,
        uid: 0,
        gid: 0,
        size: 0,
        blocks: 0,
        block_size: 4096,
        atime: 0,
        mtime: 0,
        ctime: 0,
        device: Some(device),
    }
}

pub(crate) fn sys_fstat(fd: usize, pointer: *mut u8) -> isize {
    let Some(task) = current_task() else {
        return -errno::ESRCH;
    };
    let Some(ofd) = task.fd_get(fd) else {
        return -errno::EBADF;
    };
    match ofd.inode_ref() {
        Some(inode) => match inode.metadata() {
            Ok(metadata) => copy_stat(&task, pointer, Some(metadata), 0, 0),
            Err(error) => ferr(error),
        },
        None => match &ofd.kind {
            OpenFileKind::Character(device) => copy_stat(
                &task,
                pointer,
                Some(character_metadata(device.kind())),
                0,
                0,
            ),
            OpenFileKind::Pipe(endpoint) => {
                copy_stat(&task, pointer, None, 0o010666, endpoint.pipe().object_id())
            }
            OpenFileKind::Socket(socket) => {
                copy_stat(&task, pointer, None, 0o140777, socket.object_id())
            }
            OpenFileKind::Epoll(_) => copy_stat(&task, pointer, None, 0o100600, 0),
            OpenFileKind::EventFd(_) => copy_stat(&task, pointer, None, 0o100600, 0),
            OpenFileKind::Inode(_) => unreachable!("inode_ref lost inode OFD"),
        },
    }
}

/// @description 按 Linux utimensat ABI 更新 pathname inode 的访问与修改时间。
///
/// @param fd 相对路径的目录 fd，或 AT_FDCWD；绝对路径忽略该值。
/// @param name NUL 结尾 pathname。
/// @param times 两个 RV64 timespec；空指针表示二者均取当前 realtime。
/// @param flags 仅接受 AT_SYMLINK_NOFOLLOW。
/// @return 成功返回零；路径、时间、flag、用户地址、只读或 I/O 错误返回负 errno。
pub(crate) fn sys_utimensat(
    fd: isize,
    name: *const u8,
    times: *const super::timer::TimeSpec,
    flags: u32,
) -> isize {
    const UTIME_NOW: i64 = 0x3fff_ffff;
    const UTIME_OMIT: i64 = 0x3fff_fffe;

    if flags & !AT_SYMLINK_NOFOLLOW != 0 {
        return -errno::EINVAL;
    }
    let Some(task) = current_task() else {
        return -errno::ESRCH;
    };
    let path = match path(&task, name) {
        Ok(path) => path,
        Err(error) => return error,
    };
    let start = match base(&task, fd, &path) {
        Ok(start) => start,
        Err(error) => return error,
    };
    let inode = if flags & AT_SYMLINK_NOFOLLOW != 0 {
        vfs().open_at_no_follow(start, &path, &task.access_identity(true))
    } else {
        vfs().open_at(start, &path, &task.access_identity(true))
    };
    let inode = match inode {
        Ok(inode) => inode,
        Err(error) => return ferr(error),
    };

    let now = crate::timer::get_realtime_ns() / 1_000_000_000;
    let mut owner_only = false;
    let values = if times.is_null() {
        [Some(now), Some(now)]
    } else {
        let mut bytes = [0u8; 2 * mem::size_of::<super::timer::TimeSpec>()];
        if task.copy_from_user(times as usize, &mut bytes).is_err() {
            return -errno::EFAULT;
        }
        let mut values = [None; 2];
        for (index, chunk) in bytes
            .as_chunks::<{ mem::size_of::<super::timer::TimeSpec>() }>()
            .0
            .iter()
            .enumerate()
        {
            let mut encoded = [0u8; mem::size_of::<super::timer::TimeSpec>()];
            encoded.copy_from_slice(chunk);
            let value = super::timer::decode_timespec(&encoded);
            values[index] = match value.tv_nsec {
                UTIME_NOW => Some(now),
                UTIME_OMIT => None,
                0..=999_999_999 if value.tv_sec >= 0 => {
                    owner_only = true;
                    Some(value.tv_sec as u64)
                }
                _ => return -errno::EINVAL,
            };
        }
        values
    };
    if values
        .iter()
        .flatten()
        .any(|value| *value > u32::MAX as u64)
    {
        return -errno::EOVERFLOW;
    }
    if values.iter().any(Option::is_some) {
        let metadata = match inode.metadata() {
            Ok(metadata) => metadata,
            Err(error) => return ferr(error),
        };
        let identity = task.access_identity(true);
        if identity.uid() != 0 && identity.uid() != metadata.uid {
            if owner_only {
                return -errno::EPERM;
            }
            if let Err(error) = identity.require(metadata, 2) {
                return ferr(error);
            }
        }
    }
    inode
        .set_times(values[0], values[1])
        .map_or_else(ferr, |()| 0)
}

pub(crate) fn sys_newfstatat(fd: isize, name: *const u8, pointer: *mut u8, flags: u32) -> isize {
    if flags & !AT_SYMLINK_NOFOLLOW != 0 {
        return -errno::EINVAL;
    }
    let Some(task) = current_task() else {
        return -errno::ESRCH;
    };
    let path = match path(&task, name) {
        Ok(path) => path,
        Err(error) => return error,
    };
    let start = match base(&task, fd, &path) {
        Ok(start) => start,
        Err(error) => return error,
    };
    let inode = if flags & AT_SYMLINK_NOFOLLOW != 0 {
        vfs().open_at_no_follow(start, &path, &task.access_identity(true))
    } else {
        vfs().open_at(start, &path, &task.access_identity(true))
    };
    match inode.and_then(|inode| inode.metadata()) {
        Ok(metadata) => copy_stat(&task, pointer, Some(metadata), 0, 0),
        Err(error) => ferr(error),
    }
}

pub(crate) fn sys_getdents64(fd: usize, pointer: *mut u8, length: usize) -> isize {
    let Some(task) = current_task() else {
        return -errno::ESRCH;
    };
    let Some(ofd) = task.fd_get(fd) else {
        return -errno::EBADF;
    };
    let Some(inode) = ofd.inode_ref() else {
        return -errno::ENOTDIR;
    };
    let entries = match inode.list() {
        Ok(entries) => entries,
        Err(error) => return ferr(error),
    };
    let mut index = *ofd.offset.lock() as usize;
    let mut output = Vec::new();
    while let Some(entry) = entries.get(index) {
        let record_length = (19 + entry.name.len() + 1 + 7) & !7;
        if record_length > length.saturating_sub(output.len()) {
            break;
        }
        output.extend_from_slice(&entry.inode.to_ne_bytes());
        output.extend_from_slice(&((index + 1) as i64).to_ne_bytes());
        output.extend_from_slice(&(record_length as u16).to_ne_bytes());
        output.push(match entry.kind {
            InodeType::Directory => 4,
            InodeType::Fifo => 1,
            InodeType::SymLink => 10,
            InodeType::CharacterDevice => 2,
            InodeType::File => 8,
        });
        output.extend_from_slice(&entry.name);
        output.push(0);
        output.resize(output.len() + record_length - 20 - entry.name.len(), 0);
        index += 1;
    }
    if output.is_empty() && index < entries.len() {
        return -errno::EINVAL;
    }
    if task.copy_to_user(pointer as usize, &output).is_err() {
        return -errno::EFAULT;
    }
    *ofd.offset.lock() = index as u64;
    output.len() as isize
}
pub(crate) fn sys_dup(fd: usize) -> isize {
    let Some(task) = current_task() else {
        return -errno::ESRCH;
    };
    if task.fd_get(fd).is_none() {
        return -errno::EBADF;
    }
    task.fd_duplicate(fd, 0, false)
        .map_or(-errno::EMFILE, |value| value as isize)
}
pub(crate) fn sys_dup3(old: usize, new: usize, flags: u32) -> isize {
    if old == new || flags & !O_CLOEXEC != 0 {
        return -errno::EINVAL;
    }
    let Some(task) = current_task() else {
        return -errno::ESRCH;
    };
    if new >= task.file_descriptor_limit() {
        return -errno::EBADF;
    }
    task.fd_duplicate_to(old, new, flags & O_CLOEXEC != 0)
        .map_or(-errno::EBADF, |value| value as isize)
}
pub(crate) fn sys_get_cwd(pointer: *mut u8, length: usize) -> isize {
    let Some(task) = current_task() else {
        return -errno::ESRCH;
    };
    let mut bytes = match vfs().absolute_path(task.working_directory()) {
        Ok(path) => path,
        Err(error) => return ferr(error),
    };
    bytes.push(0);
    if bytes.len() > length {
        return -errno::ERANGE;
    }
    task.copy_to_user(pointer as usize, &bytes)
        .map_or(-errno::EFAULT, |_| bytes.len() as isize)
}
