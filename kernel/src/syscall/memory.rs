use alloc::vec::Vec;

use crate::{
    fs::{InodeType, O_ACCMODE, O_RDONLY, O_WRONLY},
    memory::{MapPermission, MemoryError},
    task::current_task,
};

use super::errno;

const PROT_READ: usize = 0x1;
const PROT_WRITE: usize = 0x2;
const PROT_EXEC: usize = 0x4;
const MAP_PRIVATE: usize = 0x02;
const MAP_SHARED: usize = 0x01;
const MAP_FIXED: usize = 0x10;
const MAP_ANONYMOUS: usize = 0x20;
const MAP_FIXED_NOREPLACE: usize = 0x10_0000;

fn permission_from_prot(prot: usize) -> Result<MapPermission, isize> {
    if prot & !(PROT_READ | PROT_WRITE | PROT_EXEC) != 0 {
        return Err(errno::EINVAL);
    }
    if prot & PROT_WRITE != 0 && prot & PROT_EXEC != 0 {
        return Err(errno::EACCES);
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

/// @description 查询或设置当前进程的数据段结尾。
///
/// @param new_brk 新的数据段结尾；为零时查询当前值。
/// @return Linux `brk` 语义：成功返回新 break，失败返回未改变的旧 break。
pub(crate) fn sys_brk(new_brk: usize) -> isize {
    let task = current_task().expect("brk requires a current task");
    let current = task
        .set_program_break(0)
        .expect("user address space must own a heap area");
    task.set_program_break(new_brk).unwrap_or(current) as isize
}

/// @description 建立 Linux/riscv64 anonymous/file private 或 shared mapping。
///
/// @param address 零或地址 hint；`MAP_FIXED_NOREPLACE` 时必须页对齐且非零。
/// @param length 非零映射长度。
/// @param prot `PROT_NONE/READ/WRITE/EXEC` 子集，强制 W^X。
/// @param flags 必须选择一个 `MAP_PRIVATE/MAP_SHARED`，可附加 anonymous/fixed variants。
/// @param fd anonymous mapping 必须传 `-1`；file mapping 为 readable regular-file fd。
/// @param offset anonymous mapping 必须传零。
/// @return 成功返回映射地址；失败返回负 Linux errno。
pub(crate) fn sys_mmap(
    address: usize,
    length: usize,
    prot: usize,
    flags: usize,
    fd: isize,
    offset: usize,
) -> isize {
    let sharing = flags & (MAP_PRIVATE | MAP_SHARED);
    if !matches!(sharing, MAP_PRIVATE | MAP_SHARED)
        || flags & !(MAP_PRIVATE | MAP_SHARED | MAP_FIXED | MAP_ANONYMOUS | MAP_FIXED_NOREPLACE)
            != 0
        || flags & MAP_FIXED != 0 && flags & MAP_FIXED_NOREPLACE != 0
    {
        return -errno::EINVAL;
    }
    let permission = match permission_from_prot(prot) {
        Ok(permission) => permission,
        Err(error) => return -error,
    };
    let task = current_task().expect("mmap requires a current task");
    let fixed = flags & MAP_FIXED != 0;
    if fixed {
        if address == 0 || !address.is_multiple_of(crate::memory::PAGE_SIZE) {
            return -errno::EINVAL;
        }
        if let Err(error) = task.unmap_user_mapping(address, length) {
            return -memory_errno(error);
        }
    }
    let exact_address = fixed || flags & MAP_FIXED_NOREPLACE != 0;
    if flags & MAP_ANONYMOUS != 0 {
        if fd != -1 || offset != 0 {
            return -errno::EINVAL;
        }
        let result = if sharing == MAP_SHARED {
            task.map_shared_anonymous(address, length, permission, exact_address)
        } else {
            task.map_anonymous(address, length, permission, exact_address)
        };
        return result.map_or_else(|error| -memory_errno(error), |mapped| mapped as isize);
    }
    if fd < 0 || !offset.is_multiple_of(crate::memory::PAGE_SIZE) {
        return -errno::EINVAL;
    }
    let Some(ofd) = task.fd_get(fd as usize) else {
        return -errno::EBADF;
    };
    if *ofd.flags.lock() & O_ACCMODE == O_WRONLY {
        return -errno::EACCES;
    }
    let Some(inode) = ofd.inode_ref() else {
        return -errno::ENODEV;
    };
    if inode.inode_type() != InodeType::File {
        return -errno::ENODEV;
    }
    if sharing == MAP_SHARED {
        if permission.contains(MapPermission::W) && *ofd.flags.lock() & O_ACCMODE == O_RDONLY {
            return -errno::EACCES;
        }
        let mapping = match crate::fs::mapping(inode) {
            Ok(mapping) => mapping,
            Err(crate::fs::FileSystemError::OutOfMemory) => return -errno::ENOMEM,
            Err(_) => return -errno::EIO,
        };
        return task
            .map_shared_file(
                address,
                length,
                permission,
                exact_address,
                mapping,
                offset as u64,
            )
            .map_or_else(|error| -memory_errno(error), |mapped| mapped as isize);
    }
    let available = usize::try_from(inode.size().saturating_sub(offset as u64))
        .unwrap_or(usize::MAX)
        .min(length);
    let mut data = Vec::new();
    if data.try_reserve_exact(available).is_err() {
        return -errno::ENOMEM;
    }
    data.resize(available, 0);
    match crate::fs::read(inode, offset as u64, &mut data) {
        Ok(read) if read == available => {}
        Ok(_) | Err(_) => return -errno::EIO,
    }
    task.map_private_file(address, length, permission, exact_address, &data)
        .map_or_else(|error| -memory_errno(error), |mapped| mapped as isize)
}

/// @description 按 Linux 语义同步覆盖区间内的 file-backed MAP_SHARED mappings。
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

/// @description 解除 Linux/riscv64 anonymous private 映射，允许区间包含未映射洞。
///
/// @param address page-aligned 起始地址。
/// @param length 非零长度，向上取整到整页。
/// @return 成功返回零；非法范围或触及非 anonymous VMA 返回负 errno。
pub(crate) fn sys_munmap(address: usize, length: usize) -> isize {
    current_task()
        .expect("munmap requires a current task")
        .unmap_user_mapping(address, length)
        .map_or_else(|error| -memory_errno(error), |()| 0)
}

/// @description 修改完整 anonymous private 区间的页权限并强制 W^X。
///
/// @param address page-aligned 起始地址。
/// @param length 非零长度，向上取整到整页。
/// @param prot `PROT_NONE/READ/WRITE/EXEC` 子集。
/// @return 成功返回零；缺页、越界或权限策略失败返回负 errno。
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
