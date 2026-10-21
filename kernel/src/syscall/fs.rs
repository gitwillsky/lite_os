use crate::sync::WaitResult;
use alloc::{sync::Arc, vec::Vec};
use core::mem;

mod access;
mod attributes;
mod fcntl;
mod flock;
mod inotify;
mod io;
mod links;
pub(crate) mod mount;
mod namespace;
mod open;
mod pathname;
mod swap;
pub(crate) mod xattr;
pub(crate) use swap::{sys_swapoff, sys_swapon};
mod readlink;
pub(crate) mod statistics;
pub(crate) mod statx;
pub(crate) use access::sys_faccessat;
pub(crate) use attributes::{sys_fchmod, sys_fchmodat, sys_fchown, sys_fchownat};
pub(crate) use fcntl::sys_fcntl;
pub(crate) use flock::sys_flock;
pub(crate) use inotify::{sys_inotify_add_watch, sys_inotify_init1, sys_inotify_rm_watch};
pub(in crate::syscall) use io::{
    AsyncPositionedRead, AsyncReadStep, begin_current_position_read, begin_positioned_read,
    check_readable, check_writable, import_iovecs, perform_read, perform_write, positioned_read,
    positioned_write,
};
pub(crate) use io::{
    sys_copy_file_range, sys_pread64, sys_preadv, sys_preadv2, sys_pwrite64, sys_pwritev,
    sys_pwritev2, sys_read, sys_readv, sys_sendfile, sys_write, sys_writev,
};
pub(crate) use links::{sys_linkat, sys_symlinkat};
pub(crate) use namespace::{sys_mkdirat, sys_mknodat, sys_renameat2, sys_unlinkat};
pub(in crate::syscall) use open::open_at;
pub(crate) use open::{sys_chdir, sys_chroot, sys_fchdir, sys_openat};
use pathname::base;
pub(in crate::syscall) use pathname::ferr as filesystem_error;
pub(in crate::syscall) use pathname::ferr;
pub(in crate::syscall) use pathname::path as user_path;
pub(crate) use readlink::sys_readlinkat;

use crate::{
    file::{OpenFileDescription, OpenFileKind},
    fs::{
        Dirent64Batch, InodeMetadata, InodeType, MAX_GETDENTS_BATCH_BYTES, O_ACCMODE, O_APPEND,
        O_CLOEXEC, O_NONBLOCK, O_RDONLY, O_WRONLY, RegularFile, RegularFileWrite, vfs,
    },
    ipc::{PIPE_BUF, Pipe, PipeDirection, PipeRead, PipeWaitCondition, PipeWrite},
    syscall::errno,
    task::{TaskControlBlock, current_task, send_kernel_thread_signal, send_thread_signal},
};

const AT_FDCWD: isize = -100;
const AT_SYMLINK_NOFOLLOW: u32 = 0x100;
pub(crate) fn sys_close(fd: usize) -> isize {
    current_task().map_or(-errno::ESRCH, |t| {
        t.fd_close(fd).map_or(-errno::EBADF, |_| 0)
    })
}

/// 创建一对共享 anonymous pipe 的 Linux read/write descriptors。
///
/// # Parameters
///
/// - `descriptors`: 用户态 `int[2]` 输出地址。
/// - `flags`: 只接受 `O_CLOEXEC|O_NONBLOCK`。
///
/// # Returns
///
/// 成功返回零；参数、fd 容量、内存或 copyout 错误返回负 errno。
pub(crate) fn sys_pipe2(descriptors: usize, flags: u32) -> isize {
    if flags & !(O_CLOEXEC | O_NONBLOCK) != 0 {
        return -errno::EINVAL;
    }
    let task = current_task().expect("pipe2 requires current task");
    let reservation = match task.fd_prepare_opens::<2>(flags & O_CLOEXEC != 0) {
        Ok(reservation) => reservation,
        Err(error) => return super::file_descriptor_error(error),
    };
    let (reader, writer) = match crate::ipc::Pipe::pair() {
        Ok(pair) => pair,
        Err(()) => return -errno::ENOMEM,
    };
    let opened = match crate::fs::pipe_opened(&reader.pipe(), &task.access_identity(true)) {
        Ok(opened) => opened,
        Err(error) => return pathname::ferr(error),
    };
    let pipe_flags = flags & O_NONBLOCK;
    let read_ofd = match OpenFileDescription::pipe(reader, O_RDONLY | pipe_flags, opened.clone()) {
        Ok(ofd) => ofd,
        Err(()) => return -errno::ENOMEM,
    };
    let write_ofd = match OpenFileDescription::pipe(writer, O_WRONLY | pipe_flags, opened) {
        Ok(ofd) => ofd,
        Err(()) => return -errno::ENOMEM,
    };
    let [read_fd, write_fd] = reservation.descriptors();
    let mut output = [0u8; 8];
    output[..4].copy_from_slice(&(read_fd as i32).to_ne_bytes());
    output[4..].copy_from_slice(&(write_fd as i32).to_ne_bytes());
    if task.copy_to_user(descriptors, &output).is_err() {
        return -errno::EFAULT;
    }
    reservation.publish_many([read_ofd, write_ofd]);
    0
}

/// OFD offset owner 的可失败 acquisition 与 seek 错误；不返回伪 offset。
pub(in crate::syscall) fn position_error(error: crate::file::PositionError) -> isize {
    -match error {
        crate::file::PositionError::OutOfMemory => errno::ENOMEM,
        crate::file::PositionError::InvalidSeek | crate::file::PositionError::SameOwner => {
            errno::EINVAL
        }
    }
}

pub(crate) fn sys_lseek(fd: usize, offset: i64, whence: u32) -> isize {
    let Some(task) = current_task() else {
        return -errno::ESRCH;
    };
    let Some(ofd) = task.fd_get(fd) else {
        return -errno::EBADF;
    };
    if matches!(&ofd.kind, OpenFileKind::SignalFd(_)) {
        // Native signalfd_fops.noop_llseek 不改变实际 f_pos，连负 offset 也不作计算。
        return if whence <= 4 {
            ofd.position_snapshot()
                .map_or_else(position_error, |position| position as isize)
        } else {
            -errno::EINVAL
        };
    }
    if let OpenFileKind::Device(file) = &ofd.kind {
        if let Some(file) = file.positioned() {
            return ofd
                .with_position(|position| match file.seek(*position, offset, whence) {
                    Ok(next) => {
                        *position = next;
                        next as isize
                    }
                    Err(error) => -error.errno(),
                })
                .unwrap_or_else(position_error);
        }
        return file
            .seek(offset, whence)
            .map_or_else(|error| -error.errno(), |position| position as isize);
    }
    if !matches!(ofd.kind, OpenFileKind::Inode) {
        return -errno::ESPIPE;
    }
    let inode = ofd.inode_ref();
    if whence > 2 {
        return -errno::EINVAL;
    }
    ofd.seek_position(offset, |position| match whence {
        0 => 0,
        1 => position,
        2 => inode.size(),
        _ => unreachable!(),
    })
    .map_or_else(position_error, |position| position as isize)
}

pub(crate) fn sys_ftruncate(fd: usize, size: u64) -> isize {
    let Some(task) = current_task() else {
        return -errno::ESRCH;
    };
    let Some(ofd) = task.fd_get(fd) else {
        return -errno::EBADF;
    };
    if ofd.status_flags() & O_ACCMODE == O_RDONLY {
        return -errno::EBADF;
    }
    if size > task.file_size_limit() {
        send_kernel_thread_signal(task.tgid(), task.tid(), syscall_abi::signal::SIGXFSZ)
            .expect("current ftruncate caller must exist");
        return -errno::EFBIG;
    }
    // backend kind 只选择 I/O 操作；proc 的 regular inode 也可使用已绑定的 DeviceFile。
    // ftruncate 的准入按实际 inode 类型，之后由同一 filesystem owner 处理大小与元数据语义。
    if ofd.inode_ref().inode_type() != InodeType::File {
        return -errno::EINVAL;
    }
    let result = crate::fs::truncate(ofd.inode_ref(), size, &task.access_identity(true))
        .map_or_else(ferr, |_| 0);
    if result == 0 {
        crate::fs::notify_opened(&ofd.opened_ref(), crate::fs::IN_MODIFY);
    }
    result
}

/// 实现 Linux fallocate mode=0 的 regular-file space reservation。
///
/// # Parameters
///
/// - `fd`: 必须以 write access 打开的 regular-file descriptor。
/// - `mode`: 当前只接受零；其他 Linux allocation mode 明确返回 EOPNOTSUPP。
/// - `offset`: 非负 byte range 起点。
/// - `length`: 正数 byte range 长度。
///
/// # Returns
///
/// 成功返回零；fd、range、RLIMIT_FSIZE、空间或 I/O 错误返回负 errno。
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
    if ofd.status_flags() & O_ACCMODE == O_RDONLY {
        return -errno::EBADF;
    }
    if !matches!(ofd.kind, OpenFileKind::Inode) {
        return -errno::ENODEV;
    }
    let inode = ofd.inode_ref();
    if inode.inode_type() == InodeType::Directory {
        return -errno::EISDIR;
    }
    if inode.inode_type() != InodeType::File {
        return -errno::ENODEV;
    }
    if end > task.file_size_limit() {
        send_kernel_thread_signal(task.tgid(), task.tid(), syscall_abi::signal::SIGXFSZ)
            .expect("current fallocate caller must exist");
        return -errno::EFBIG;
    }
    let result = crate::fs::allocate(
        inode,
        offset as u64,
        length as u64,
        &task.access_identity(true),
    )
    .map_or_else(ferr, |_| 0);
    if result == 0 {
        crate::fs::notify_opened(&ofd.opened_ref(), crate::fs::IN_MODIFY);
    }
    result
}

pub(super) fn sync_file(fd: usize) -> isize {
    let Some(task) = current_task() else {
        return -errno::ESRCH;
    };
    let Some(ofd) = task.fd_get(fd) else {
        return -errno::EBADF;
    };
    sync_ofd(&ofd)
}

/// 提交一个 inode-backed OFD 的数据与 metadata；io_uring 的 worker 与 `fsync(2)` 共用。
///
/// # Returns
///
/// 成功零；非 inode OFD `EINVAL`；底层 I/O 失败返回对应负 errno。
pub(super) fn sync_ofd(ofd: &OpenFileDescription) -> isize {
    if !matches!(ofd.kind, OpenFileKind::Inode) {
        return -errno::EINVAL;
    }
    crate::fs::sync_inode(ofd.inode_ref()).map_or_else(ferr, |_| 0)
}

/// 把一个 inode-backed OFD 的数据与 metadata 提交到 stable storage。
///
/// # Parameters
///
/// - `fd`: 要同步的 descriptor。
///
/// # Returns
///
/// 成功返回零；非 inode fd 或底层 I/O 失败返回负 errno。
pub(crate) fn sys_fsync(fd: usize) -> isize {
    sync_file(fd)
}

/// 提交文件数据及恢复该数据所需 metadata；当前同步 journal 模型与 fsync 共用提交边界。
///
/// # Parameters
///
/// - `fd`: 要同步的 descriptor。
///
/// # Returns
///
/// 成功返回零；非 inode fd 或底层 I/O 失败返回负 errno。
pub(crate) fn sys_fdatasync(fd: usize) -> isize {
    sync_file(fd)
}

/// 将唯一 mounted filesystem 的已提交写入同步到 stable storage。
///
/// # Returns
///
/// 按 Linux sync ABI 固定返回零；单个 writeback error 不通过该入口报告。
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

fn copy_stat(task: &TaskControlBlock, pointer: *mut u8, metadata: InodeMetadata) -> isize {
    let stat = UserStat {
        st_dev: metadata.filesystem,
        st_ino: metadata.inode,
        st_mode: metadata.mode,
        st_nlink: metadata.links,
        st_uid: task.user_namespace().display_id(true, metadata.uid),
        st_gid: task.user_namespace().display_id(false, metadata.gid),
        st_rdev: metadata.device.map_or(0, crate::fs::DeviceNumber::encode),
        pad1: 0,
        st_size: metadata.size as i64,
        st_blksize: metadata.block_size as i32,
        pad2: 0,
        st_blocks: metadata.blocks as i64,
        st_atime: metadata.atime as i64,
        st_atime_nsec: u64::from(metadata.fine_times.atime_nsec),
        st_mtime: metadata.mtime as i64,
        st_mtime_nsec: u64::from(metadata.fine_times.mtime_nsec),
        st_ctime: metadata.ctime as i64,
        st_ctime_nsec: u64::from(metadata.fine_times.ctime_nsec),
        unused: [0; 2],
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

pub(crate) fn sys_fstat(fd: usize, pointer: *mut u8) -> isize {
    let Some(task) = current_task() else {
        return -errno::ESRCH;
    };
    let Some(ofd) = task.fd_get_raw(fd) else {
        return -errno::EBADF;
    };
    match ofd.inode_ref().metadata() {
        Ok(metadata) => copy_stat(&task, pointer, metadata),
        Err(error) => ferr(error),
    }
}

/// 按 Linux utimensat ABI 更新 pathname inode 的访问与修改时间。
///
/// # Parameters
///
/// - `fd`: 相对路径的目录 fd，或 AT_FDCWD；绝对路径忽略该值。
/// - `name`: NUL 结尾 pathname；NULL 以 fd 操作（futimens），空字符串需 AT_EMPTY_PATH。
/// - `times`: 两个 LP64 timespec；空指针表示二者均取当前 realtime。
/// - `flags`: pathname 接受 AT_SYMLINK_NOFOLLOW|AT_EMPTY_PATH；NULL fd 路由要求零。
///
/// # Returns
///
/// 成功返回零；路径、时间、flag、用户地址、只读或 I/O 错误返回负 errno。
pub(crate) fn sys_utimensat(
    fd: isize,
    name: *const u8,
    times: *const super::timer::TimeSpec,
    flags: u32,
) -> isize {
    const UTIME_NOW: i64 = 0x3fff_ffff;
    const UTIME_OMIT: i64 = 0x3fff_fffe;

    const AT_EMPTY_PATH: u32 = 0x1000;
    let Some(task) = current_task() else {
        return -errno::ESRCH;
    };
    // Linux 首先 copy 两个 timespec；双 OMIT 不访问 pathname、fd 或 flags。
    let encoded_times = if times.is_null() {
        None
    } else {
        let mut bytes = [0u8; 2 * mem::size_of::<super::timer::TimeSpec>()];
        if task.copy_from_user(times as usize, &mut bytes).is_err() {
            return -errno::EFAULT;
        }
        let size = mem::size_of::<super::timer::TimeSpec>();
        let values = [
            super::timer::decode_timespec(&bytes[..size].try_into().unwrap()),
            super::timer::decode_timespec(&bytes[size..].try_into().unwrap()),
        ];
        if values.iter().all(|value| value.tv_nsec == UTIME_OMIT) {
            return 0;
        }
        Some(values)
    };
    if flags & !(AT_SYMLINK_NOFOLLOW | AT_EMPTY_PATH) != 0 {
        return -errno::EINVAL;
    }
    let opened = if name.is_null() && fd != AT_FDCWD {
        // futimens 的标准 utimensat(fd, NULL, times, 0) 路由；不是 pathname 备用路径。
        if flags != 0 {
            return -errno::EINVAL;
        }
        match usize::try_from(fd).ok().and_then(|fd| task.fd_get(fd)) {
            Some(ofd) => ofd.opened_ref(),
            None => return -errno::EBADF,
        }
    } else {
        let path = match pathname::path_allow_empty(&task, name) {
            Ok(path) => path,
            Err(error) => return error,
        };
        if path.is_empty() {
            if flags & AT_EMPTY_PATH == 0 {
                return -errno::ENOENT;
            }
            if fd == AT_FDCWD {
                task.lookup_origin().into_parts().1
            } else {
                match usize::try_from(fd).ok().and_then(|fd| task.fd_get_raw(fd)) {
                    Some(ofd) => ofd.opened_ref(),
                    None => return -errno::EBADF,
                }
            }
        } else {
            let start = match base(&task, fd, &path) {
                Ok(start) => start,
                Err(error) => return error,
            };
            let identity = task.access_identity(true);
            let result = if flags & AT_SYMLINK_NOFOLLOW != 0 {
                vfs().open_file_at_no_follow(start, &path, &identity)
            } else {
                vfs().open_file_at(start, &path, &identity)
            };
            match result {
                Ok(opened) => opened,
                Err(error) => return ferr(error),
            }
        }
    };
    let inode = opened.inode();

    let now = crate::timer::get_realtime_ns() / 1_000_000_000;
    let owner_only =
        encoded_times.is_some_and(|values| !values.iter().all(|value| value.tv_nsec == UTIME_NOW));
    let values = if let Some(encoded) = encoded_times {
        let mut values = [None; 2];
        for (index, value) in encoded.iter().enumerate() {
            values[index] = match value.tv_nsec {
                UTIME_NOW => Some(now),
                UTIME_OMIT => None,
                0..=999_999_999 if value.tv_sec >= 0 => Some(value.tv_sec as u64),
                _ => return -errno::EINVAL,
            };
        }
        values
    } else {
        [Some(now), Some(now)]
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
        if !identity.owns_inode(metadata.uid) {
            if owner_only {
                return -errno::EPERM;
            }
            if let Err(error) = identity.require(metadata, 2) {
                return ferr(error);
            }
        }
    }
    let _writer = if values.iter().any(Option::is_some) {
        match opened.begin_write() {
            Ok(writer) => Some(writer),
            Err(error) => return ferr(error),
        }
    } else {
        None
    };
    let result = inode.set_times(values[0], values[1]);
    if result.is_ok() {
        crate::fs::notify_opened(&opened, crate::fs::IN_ATTRIB);
    }
    result.map_or_else(ferr, |()| 0)
}

/// stat 与 statx 共用的目标解析：返回目标 inode 的 metadata 与所在 mount 的 identity。
///
/// # Parameters
///
/// - `fd` / `name`: `*at` 语义的目录 fd 与 pathname；`name` 为 NULL 且 `empty_path` 时直接操作 `fd`。
/// - `nofollow`: 不跟随最终 symbolic link。
/// - `empty_path`: 允许空 pathname（`AT_EMPTY_PATH`）。
///
/// # Returns
///
/// 成功返回 `(metadata, mount_id)`；失败返回负 errno。
pub(super) fn resolve_stat_target(
    task: &TaskControlBlock,
    fd: isize,
    name: *const u8,
    nofollow: bool,
    empty_path: bool,
) -> Result<(InodeMetadata, u64), isize> {
    let path = if name.is_null() && empty_path {
        Vec::new()
    } else {
        pathname::path_allow_empty(task, name)?
    };
    let opened = if path.is_empty() {
        if !empty_path {
            return Err(-errno::ENOENT);
        }
        if fd == AT_FDCWD {
            task.lookup_origin().into_parts().1
        } else {
            usize::try_from(fd)
                .ok()
                .and_then(|fd| task.fd_get_raw(fd))
                .ok_or(-errno::EBADF)?
                .opened_ref()
        }
    } else {
        let start = base(task, fd, &path)?;
        let identity = task.access_identity(true);
        if nofollow {
            vfs().open_file_at_no_follow(start, &path, &identity)
        } else {
            vfs().open_file_at(start, &path, &identity)
        }
        .map_err(ferr)?
    };
    let metadata = opened.inode().metadata().map_err(ferr)?;
    Ok((metadata, opened.mount_id()))
}

pub(crate) fn sys_newfstatat(fd: isize, name: *const u8, pointer: *mut u8, flags: u32) -> isize {
    const AT_EMPTY_PATH: u32 = 0x1000;
    if flags & !(AT_SYMLINK_NOFOLLOW | AT_EMPTY_PATH) != 0 {
        return -errno::EINVAL;
    }
    let Some(task) = current_task() else {
        return -errno::ESRCH;
    };
    match resolve_stat_target(
        &task,
        fd,
        name,
        flags & AT_SYMLINK_NOFOLLOW != 0,
        flags & AT_EMPTY_PATH != 0,
    ) {
        Ok((metadata, _)) => copy_stat(&task, pointer, metadata),
        Err(error) => error,
    }
}

pub(crate) fn sys_getdents64(fd: usize, pointer: *mut u8, length: usize) -> isize {
    let Some(task) = current_task() else {
        return -errno::ESRCH;
    };
    let Some(ofd) = task.fd_get(fd) else {
        return -errno::EBADF;
    };
    let inode = ofd.inode_ref();
    let mut output = match Dirent64Batch::try_new(length.min(MAX_GETDENTS_BATCH_BYTES)) {
        Ok(output) => output,
        Err(error) => return ferr(error),
    };
    ofd.with_position(|position| {
        let read = match inode.read_directory(*position, &mut output) {
            Ok(read) => read,
            Err(error) => return ferr(error),
        };
        ofd.touch_access();
        ofd.notify(crate::fs::IN_ACCESS);
        if output.is_empty() && !read.eof {
            return -errno::EINVAL;
        }
        if task
            .copy_to_user(pointer as usize, output.as_slice())
            .is_err()
        {
            return -errno::EFAULT;
        }
        *position = read.cursor;
        output.as_slice().len() as isize
    })
    .unwrap_or_else(position_error)
}
pub(crate) fn sys_dup(fd: usize) -> isize {
    let Some(task) = current_task() else {
        return -errno::ESRCH;
    };
    if task.fd_get_raw(fd).is_none() {
        return -errno::EBADF;
    }
    task.fd_duplicate(fd, 0, false)
        .map_or_else(super::file_descriptor_error, |value| value as isize)
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
        .map_or_else(super::file_descriptor_error, |value| value as isize)
}
pub(crate) fn sys_get_cwd(pointer: *mut u8, length: usize) -> isize {
    let Some(task) = current_task() else {
        return -errno::ESRCH;
    };
    let mut bytes = match vfs().current_directory_path(task.lookup_origin()) {
        Ok(path) => path,
        Err(error) => return ferr(error),
    };
    if bytes.try_reserve(1).is_err() {
        return -errno::ENOMEM;
    }
    bytes.push(0);
    if bytes.len() > length {
        return -errno::ERANGE;
    }
    task.copy_to_user(pointer as usize, &bytes)
        .map_or(-errno::EFAULT, |_| bytes.len() as isize)
}
