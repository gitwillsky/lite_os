use crate::{
    file::OpenFileKind,
    fs::{InodeType, O_ACCMODE, O_RDONLY, O_WRONLY},
    memory::{FileMappingError, FileMappingSource, MapPermission, MemoryAdvice, MemoryError},
    task::current_task,
};

use super::{
    errno,
    mmap_flags::{
        MAP_ANONYMOUS, MAP_FIXED, MAP_FIXED_NOREPLACE, MAP_PRIVATE, MAP_SHARED,
        mmap_flags_supported,
    },
};

const PROT_READ: usize = 0x1;
const PROT_WRITE: usize = 0x2;
const PROT_EXEC: usize = 0x4;

fn permission_from_prot(prot: usize) -> Result<MapPermission, isize> {
    if prot & !(PROT_READ | PROT_WRITE | PROT_EXEC) != 0 {
        return Err(errno::EINVAL);
    }
    let mut permission = MapPermission::U;
    if prot & PROT_READ != 0 || prot & PROT_WRITE != 0 {
        // RISC-V 不支持 W-only leaf；Linux 的 PROT_WRITE 也允许读取该映射。
        permission |= MapPermission::R;
    }
    if prot & PROT_WRITE != 0 {
        permission |= MapPermission::W;
    }
    if prot & PROT_EXEC != 0 {
        permission |= MapPermission::X;
    }
    Ok(permission)
}

fn memory_errno(error: MemoryError) -> isize {
    if error.is_out_of_memory() {
        return errno::ENOMEM;
    }
    match error {
        MemoryError::AddressInUse => errno::EEXIST,
        MemoryError::PermissionDenied => errno::EACCES,
        MemoryError::Io => errno::EIO,
        MemoryError::InvalidRange | MemoryError::PageTableError(_) | MemoryError::OutOfMemory => {
            errno::EINVAL
        }
    }
}

/// 查询或设置当前进程的数据段结尾。
///
/// # Parameters
///
/// - `new_brk`: 新的数据段结尾；为零时查询当前值。
///
/// # Returns
///
/// Linux `brk` 语义：成功返回新 break，失败返回未改变的旧 break。
pub(crate) fn sys_brk(new_brk: usize) -> isize {
    let task = current_task().expect("brk requires a current task");
    let current = task
        .set_program_break(0)
        .expect("user address space must own a heap area");
    task.set_program_break(new_brk).unwrap_or(current) as isize
}

/// 建立 Linux 64-bit anonymous/file private 或 shared mapping。
///
/// # Parameters
///
/// - `address`: 零或地址 hint；`MAP_FIXED_NOREPLACE` 时必须页对齐且非零。
/// - `length`: 非零映射长度。
/// - `prot`: `PROT_NONE/READ/WRITE/EXEC` 的任意合法组合。
/// - `flags`: 必须选择一个 `MAP_PRIVATE/MAP_SHARED`，可附加已声明的 semantic/advisory variants。
/// - `fd`: anonymous mapping 必须传 `-1`；file mapping 为 readable regular-file fd。
/// - `offset`: anonymous mapping 必须传零。
///
/// # Returns
///
/// 成功返回映射地址；失败返回负 Linux errno。
pub(crate) fn sys_mmap(
    address: usize,
    length: usize,
    prot: usize,
    flags: usize,
    fd: isize,
    offset: usize,
) -> isize {
    let sharing = flags & (MAP_PRIVATE | MAP_SHARED);
    if !mmap_flags_supported(flags) {
        return -errno::EINVAL;
    }
    let permission = match permission_from_prot(prot) {
        Ok(permission) => permission,
        Err(error) => return -error,
    };
    let fixed = flags & MAP_FIXED != 0;
    // Reject cheap range errors before regular-file backing acquisition can publish a new
    // canonical page-cache owner. MemorySet remains the final full-range validator.
    if length == 0 {
        return -errno::EINVAL;
    }
    if fixed && (address == 0 || !address.is_multiple_of(crate::memory::PAGE_SIZE)) {
        return -errno::EINVAL;
    }
    let task = current_task().expect("mmap requires a current task");

    enum PreparedMapping {
        Anonymous,
        SharedAnonymous,
        Device(crate::memory::DeviceMappingSource),
        PrivateFile(FileMappingSource),
        SharedFile(FileMappingSource),
    }

    // Resolve every remaining ABI/backing error before destructive MAP_FIXED replacement.
    // The prepared value only retains the canonical immutable backing owner.
    let prepared = if flags & MAP_ANONYMOUS != 0 {
        if fd != -1 || offset != 0 {
            return -errno::EINVAL;
        }
        if sharing == MAP_SHARED {
            PreparedMapping::SharedAnonymous
        } else {
            PreparedMapping::Anonymous
        }
    } else {
        if fd < 0 || !offset.is_multiple_of(crate::memory::PAGE_SIZE) {
            return -errno::EINVAL;
        }
        let Some(ofd) = task.fd_get(fd as usize) else {
            return -errno::EBADF;
        };
        let access_mode = ofd.status_flags() & O_ACCMODE;
        if access_mode == O_WRONLY {
            return -errno::EACCES;
        }
        if let OpenFileKind::Device(file) = &ofd.kind {
            let request = super::device::map_request(
                sharing == MAP_SHARED,
                permission.contains(MapPermission::W),
                permission.contains(MapPermission::X),
                access_mode != O_RDONLY,
            );
            match file.mmap(offset as u64, length, request) {
                Ok(source) => PreparedMapping::Device(source),
                Err(error) => return super::device::device_error(error),
            }
        } else {
            let Some(inode) = ofd.inode_ref() else {
                return -errno::ENODEV;
            };
            if !matches!(inode.inode_type(), InodeType::File | InodeType::BlockDevice) {
                return -errno::ENODEV;
            }
            if sharing == MAP_SHARED
                && permission.contains(MapPermission::W)
                && access_mode == O_RDONLY
            {
                return -errno::EACCES;
            }
            // Linux `mmap_region`：noexec 挂载上的文件不可映射为可执行。
            if permission.contains(MapPermission::X)
                && crate::fs::vfs().mount_flags(inode.filesystem_id()).noexec()
            {
                return -errno::EPERM;
            }
            let mapping = match crate::fs::mapping(inode.clone(), ofd.opened_ref()) {
                Ok(mapping) => mapping,
                Err(crate::fs::FileSystemError::OutOfMemory) => return -errno::ENOMEM,
                Err(_) => return -errno::EIO,
            };
            let source = match FileMappingSource::new(mapping, offset as u64, length) {
                Ok(source) => source,
                Err(FileMappingError::Invalid) => return -errno::EINVAL,
                Err(FileMappingError::Overflow) => return -errno::EOVERFLOW,
            };
            if sharing == MAP_SHARED {
                PreparedMapping::SharedFile(source)
            } else {
                PreparedMapping::PrivateFile(source)
            }
        }
    };

    if fixed {
        // This begins the replacement stage. A later VMA installation error does not restore
        // the removed mapping, matching the deliberately limited Linux-compatible contract.
        if let Err(error) = task.unmap_user_mapping(address, length) {
            return -memory_errno(error);
        }
    }
    let exact_address = fixed || flags & MAP_FIXED_NOREPLACE != 0;
    let result = match prepared {
        PreparedMapping::Anonymous => {
            task.map_anonymous(address, length, permission, exact_address)
        }
        PreparedMapping::SharedAnonymous => {
            task.map_shared_anonymous(address, length, permission, exact_address)
        }
        PreparedMapping::Device(source) => {
            task.map_device(address, length, permission, exact_address, source)
        }
        PreparedMapping::PrivateFile(source) => {
            task.map_private_file(address, permission, exact_address, source)
        }
        PreparedMapping::SharedFile(source) => {
            task.map_shared_file(address, permission, exact_address, source)
        }
    };
    result.map_or_else(|error| -memory_errno(error), |mapped| mapped as isize)
}

/// 按 Linux 语义同步覆盖区间内的 file-backed MAP_SHARED mappings。
pub(crate) fn sys_msync(address: usize, length: usize, flags: usize) -> isize {
    const MS_ASYNC: usize = 1;
    const MS_INVALIDATE: usize = 2;
    const MS_SYNC: usize = 4;

    if flags & !(MS_ASYNC | MS_INVALIDATE | MS_SYNC) != 0
        || flags & MS_ASYNC != 0 && flags & MS_SYNC != 0
        || !address.is_multiple_of(crate::memory::PAGE_SIZE)
    {
        return -errno::EINVAL;
    }
    if length == 0 {
        return 0;
    }
    if flags & MS_SYNC == 0 {
        return current_task()
            .expect("msync requires a current task")
            .sync_shared_mapping(address, length, false)
            .map_or_else(|error| -msync_errno(error), |()| 0);
    }
    current_task()
        .expect("msync requires a current task")
        .sync_shared_mapping(address, length, true)
        .map_or_else(|error| -msync_errno(error), |()| 0)
}

fn msync_errno(error: MemoryError) -> isize {
    match error {
        MemoryError::InvalidRange | MemoryError::OutOfMemory => errno::ENOMEM,
        MemoryError::Io => errno::EIO,
        other => memory_errno(other),
    }
}

/// 解除 Linux 64-bit anonymous private 映射，允许区间包含未映射洞。
///
/// # Parameters
///
/// - `address`: page-aligned 起始地址。
/// - `length`: 非零长度，向上取整到整页。
///
/// # Returns
///
/// 成功返回零；非法范围或触及非 anonymous VMA 返回负 errno。
pub(crate) fn sys_munmap(address: usize, length: usize) -> isize {
    current_task()
        .expect("munmap requires a current task")
        .unmap_user_mapping(address, length)
        .map_or_else(|error| -memory_errno(error), |()| 0)
}

/// 修改完整用户 VMA 区间的页权限，保留 Linux 对合法 `PROT_*` 组合的语义。
///
/// # Parameters
///
/// - `address`: page-aligned 起始地址。
/// - `length`: 非零长度，向上取整到整页。
/// - `prot`: `PROT_NONE/READ/WRITE/EXEC` 子集。
///
/// # Returns
///
/// 成功返回零；缺页、越界或权限策略失败返回负 errno。
pub(crate) fn sys_mprotect(address: usize, length: usize, prot: usize) -> isize {
    let permission = match permission_from_prot(prot) {
        Ok(permission) => permission,
        Err(error) => return -error,
    };
    current_task()
        .expect("mprotect requires a current task")
        .protect_user_mapping(address, length, permission)
        .map_or_else(|error| -memory_errno(error), |()| 0)
}

/// 应用 Linux 64-bit madvise residency policy，不维护 syscall 层 shadow state。
pub(crate) fn sys_madvise(address: usize, length: usize, advice: usize) -> isize {
    if !address.is_multiple_of(crate::memory::PAGE_SIZE) {
        return -errno::EINVAL;
    }
    let advice = match advice {
        0 => MemoryAdvice::Normal,
        1 => MemoryAdvice::Random,
        2 => MemoryAdvice::Sequential,
        3 => MemoryAdvice::WillNeed,
        4 => MemoryAdvice::DontNeed,
        8 => MemoryAdvice::Free,
        _ => return -errno::EINVAL,
    };
    current_task()
        .expect("madvise requires a current task")
        .advise_user_mapping(address, length, advice)
        .map_or_else(|error| -memory_errno(error), |()| 0)
}
