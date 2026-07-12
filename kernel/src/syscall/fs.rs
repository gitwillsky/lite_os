use alloc::vec::Vec;
use core::mem;

mod io;
mod pathname;
mod readlink;
pub(crate) use io::{sys_read, sys_readv, sys_write, sys_writev};
use pathname::{base, ferr, path};
pub(crate) use readlink::sys_readlinkat;

use crate::{
    fs::{
        CharacterDevice, DeviceKind, FileSystemError, InodeMetadata, InodeType,
        MAX_FILE_DESCRIPTORS, O_ACCMODE, O_APPEND, O_CLOEXEC, O_NONBLOCK, O_RDONLY, O_WRONLY,
        OpenFileDescription, OpenFileKind, TerminalAccess, TerminalRead, vfs,
    },
    ipc::{PIPE_BUF, PipeDirection, PipeRead, PipeWrite},
    syscall::errno,
    task::{
        TaskControlBlock, WaitResult, create_pipe_endpoints, current_task, drain_terminal_input,
        send_thread_signal, session_id, wait_for_pipe,
    },
};

use super::tty::guard_terminal_access;

const AT_FDCWD: isize = -100;
const AT_REMOVEDIR: usize = 0x200;
const AT_SYMLINK_NOFOLLOW: u32 = 0x100;
const O_CREAT: u32 = 0x40;
const O_EXCL: u32 = 0x80;
const O_TRUNC: u32 = 0x200;
const O_DIRECTORY: u32 = 0x10000;

/// @description 将当前 Process 工作目录切换到 Linux pathname 指定的目录。
///
/// @param name NUL 结尾的 raw pathname；相对路径从当前 cwd inode 解析。
/// @return 成功返回零；用户指针、路径、类型、I/O 或内存错误返回负 errno。
pub(crate) fn sys_chdir(name: *const u8) -> isize {
    let Some(task) = current_task() else {
        return -errno::ESRCH;
    };
    let path = match path(&task, name) {
        Ok(path) => path,
        Err(error) => return error,
    };
    let start = (path.first() != Some(&b'/')).then(|| task.working_directory());
    let inode = match vfs().open_at(start, &path) {
        Ok(inode) => inode,
        Err(error) => return ferr(error),
    };
    if inode.inode_type() != InodeType::Directory {
        return -errno::ENOTDIR;
    }
    task.set_working_directory(inode);
    0
}

pub(crate) fn sys_openat(fd: isize, name: *const u8, flags: u32, mode: u32) -> isize {
    let Some(task) = current_task() else {
        return -errno::ESRCH;
    };
    let path = match path(&task, name) {
        Ok(v) => v,
        Err(e) => return e,
    };
    if flags & O_ACCMODE == O_ACCMODE {
        return -errno::EINVAL;
    }
    let start = match base(&task, fd, &path) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let inode = match vfs().open_at(start.clone(), &path) {
        Ok(_) if flags & O_CREAT != 0 && flags & O_EXCL != 0 => return -errno::EEXIST,
        Ok(v) => v,
        Err(FileSystemError::NotFound) if flags & O_CREAT != 0 => {
            if path.last() == Some(&b'/') {
                return -errno::ENOTDIR;
            }
            match vfs().create_at(start, &path, InodeType::File, mode) {
                Ok(v) => v,
                Err(e) => return ferr(e),
            }
        }
        Err(e) => return ferr(e),
    };
    if flags & O_DIRECTORY != 0 && inode.inode_type() != InodeType::Directory {
        return -errno::ENOTDIR;
    }
    if inode.inode_type() == InodeType::Directory && flags & O_ACCMODE != O_RDONLY {
        return -errno::EISDIR;
    }
    if !matches!(
        inode.inode_type(),
        InodeType::File | InodeType::Directory | InodeType::CharacterDevice
    ) || inode.inode_type() == InodeType::CharacterDevice && inode.device_kind().is_none()
    {
        return -errno::ENXIO;
    }
    let ofd_flags = flags & !(O_CREAT | O_EXCL | O_TRUNC | O_CLOEXEC);
    let ofd = if let Some(device) = inode.device_kind() {
        let terminal = task.terminal();
        if device == DeviceKind::Tty {
            let Ok(session) = session_id(0) else {
                return -errno::ENXIO;
            };
            if terminal.controlling_session() != Some(session) {
                return -errno::ENXIO;
            }
        }
        OpenFileDescription::character(device, terminal, ofd_flags)
    } else {
        if flags & O_TRUNC != 0
            && flags & O_ACCMODE != O_RDONLY
            && let Err(error) = inode.truncate(0)
        {
            return ferr(error);
        }
        OpenFileDescription::inode(inode, ofd_flags)
    };
    task.fd_allocate(ofd, flags & O_CLOEXEC != 0)
        .map_or(-errno::EMFILE, |v| v as isize)
}

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

pub(crate) fn sys_mkdirat(fd: isize, name: *const u8, mode: u32) -> isize {
    let Some(task) = current_task() else {
        return -errno::ESRCH;
    };
    let p = match path(&task, name) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let b = match base(&task, fd, &p) {
        Ok(v) => v,
        Err(e) => return e,
    };
    vfs()
        .create_at(b, &p, InodeType::Directory, mode)
        .map_or_else(ferr, |_| 0)
}
pub(crate) fn sys_unlinkat(fd: isize, name: *const u8, flags: usize) -> isize {
    if flags & !AT_REMOVEDIR != 0 {
        return -errno::EINVAL;
    }
    let Some(task) = current_task() else {
        return -errno::ESRCH;
    };
    let p = match path(&task, name) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let b = match base(&task, fd, &p) {
        Ok(v) => v,
        Err(e) => return e,
    };
    vfs()
        .unlink_at(b, &p, flags & AT_REMOVEDIR != 0)
        .map_or_else(ferr, |_| 0)
}
pub(crate) fn sys_renameat2(
    ofd: isize,
    on: *const u8,
    nfd: isize,
    nn: *const u8,
    flags: u32,
) -> isize {
    if flags & !1 != 0 {
        return -errno::EINVAL;
    }
    let Some(task) = current_task() else {
        return -errno::ESRCH;
    };
    let op = match path(&task, on) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let np = match path(&task, nn) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let ob = match base(&task, ofd, &op) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let nb = match base(&task, nfd, &np) {
        Ok(v) => v,
        Err(e) => return e,
    };
    vfs()
        .rename_at(ob, &op, nb, &np, flags & 1 != 0)
        .map_or_else(ferr, |_| 0)
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
    ofd.inode_ref()
        .ok_or(-errno::EINVAL)
        .and_then(|i| i.truncate(size).map_err(ferr))
        .map_or_else(|e| e, |_| 0)
}
pub(crate) fn sys_fsync(fd: usize) -> isize {
    let Some(task) = current_task() else {
        return -errno::ESRCH;
    };
    let Some(ofd) = task.fd_get(fd) else {
        return -errno::EBADF;
    };
    ofd.inode_ref()
        .map_or(-errno::EINVAL, |i| i.sync().map_or_else(ferr, |_| 0))
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

fn copy_stat(task: &TaskControlBlock, pointer: *mut u8, metadata: Option<InodeMetadata>) -> isize {
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
            st_ino: 0,
            st_mode: 0o020666,
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
            Ok(metadata) => copy_stat(&task, pointer, Some(metadata)),
            Err(error) => ferr(error),
        },
        None => match &ofd.kind {
            OpenFileKind::Character(device) => {
                copy_stat(&task, pointer, Some(character_metadata(device.kind())))
            }
            OpenFileKind::Pipe(_) => copy_stat(&task, pointer, None),
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
        vfs().open_at_no_follow(start, &path)
    } else {
        vfs().open_at(start, &path)
    };
    let inode = match inode {
        Ok(inode) => inode,
        Err(error) => return ferr(error),
    };

    let now = crate::timer::get_realtime_ns() / 1_000_000_000;
    let values = if times.is_null() {
        [Some(now), Some(now)]
    } else {
        let mut bytes = [0u8; 2 * mem::size_of::<super::timer::TimeSpec>()];
        if task.copy_from_user(times as usize, &mut bytes).is_err() {
            return -errno::EFAULT;
        }
        let mut values = [None; 2];
        for (index, chunk) in bytes
            .chunks_exact(mem::size_of::<super::timer::TimeSpec>())
            .enumerate()
        {
            let mut encoded = [0u8; mem::size_of::<super::timer::TimeSpec>()];
            encoded.copy_from_slice(chunk);
            let value = super::timer::decode_timespec(&encoded);
            values[index] = match value.tv_nsec {
                UTIME_NOW => Some(now),
                UTIME_OMIT => None,
                0..=999_999_999 if value.tv_sec >= 0 => Some(value.tv_sec as u64),
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
        vfs().open_at_no_follow(start, &path)
    } else {
        vfs().open_at(start, &path)
    };
    match inode.and_then(|inode| inode.metadata()) {
        Ok(metadata) => copy_stat(&task, pointer, Some(metadata)),
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
    if new >= MAX_FILE_DESCRIPTORS {
        return -errno::EBADF;
    }
    let Some(task) = current_task() else {
        return -errno::ESRCH;
    };
    task.fd_duplicate_to(old, new, flags & O_CLOEXEC != 0)
        .map_or(-errno::EBADF, |value| value as isize)
}
pub(crate) fn sys_fcntl(fd: usize, cmd: u32, arg: usize) -> isize {
    let Some(t) = current_task() else {
        return -errno::ESRCH;
    };
    match cmd {
        0 if arg < MAX_FILE_DESCRIPTORS => {
            if t.fd_get(fd).is_none() {
                -errno::EBADF
            } else {
                t.fd_duplicate(fd, arg, false)
                    .map_or(-errno::EMFILE, |value| value as isize)
            }
        }
        1 => t.fd_flags(fd).map_or(-errno::EBADF, |v| v as isize),
        2 => t.fd_set_flags(fd, arg as u32).map_or(-errno::EBADF, |_| 0),
        3 => t
            .fd_get(fd)
            .map_or(-errno::EBADF, |v| *v.flags.lock() as isize),
        4 => t.fd_get(fd).map_or(-errno::EBADF, |ofd| {
            let mut flags = ofd.flags.lock();
            *flags = (*flags & !O_APPEND) | (arg as u32 & O_APPEND);
            0
        }),
        1030 if arg < MAX_FILE_DESCRIPTORS => {
            if t.fd_get(fd).is_none() {
                -errno::EBADF
            } else {
                t.fd_duplicate(fd, arg, true)
                    .map_or(-errno::EMFILE, |value| value as isize)
            }
        }
        _ => -errno::EINVAL,
    }
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
