use crate::{
    memory::{MapPermission, MemoryError},
    task::current_task,
};

use super::errno;

const PROT_READ: usize = 0x1;
const PROT_WRITE: usize = 0x2;
const PROT_EXEC: usize = 0x4;
const MAP_PRIVATE: usize = 0x02;
const MAP_ANONYMOUS: usize = 0x20;
const MAP_FIXED_NOREPLACE: usize = 0x10_0000;

fn permission_from_prot(prot: usize) -> Result<MapPermission, isize> {
    if prot & !(PROT_READ | PROT_WRITE | PROT_EXEC) != 0 || prot == 0 {
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

/// @description 建立 Linux/riscv64 anonymous private eager mapping。
///
/// @param address 零或地址 hint；`MAP_FIXED_NOREPLACE` 时必须页对齐且非零。
/// @param length 非零映射长度。
/// @param prot `PROT_READ/WRITE/EXEC` 子集；当前不支持 `PROT_NONE`，强制 W^X。
/// @param flags 必须为 `MAP_PRIVATE|MAP_ANONYMOUS`，可附加 `MAP_FIXED_NOREPLACE`。
/// @param fd anonymous mapping 必须传 `-1`。
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
    let required = MAP_PRIVATE | MAP_ANONYMOUS;
    if flags & required != required
        || flags & !(required | MAP_FIXED_NOREPLACE) != 0
        || fd != -1
        || offset != 0
    {
        return -errno::EINVAL;
    }
    let permission = match permission_from_prot(prot) {
        Ok(permission) => permission,
        Err(error) => return -error,
    };
    current_task()
        .expect("mmap requires a current task")
        .map_anonymous(
            address,
            length,
            permission,
            flags & MAP_FIXED_NOREPLACE != 0,
        )
        .map_or_else(|error| -memory_errno(error), |mapped| mapped as isize)
}

/// @description 解除 Linux/riscv64 anonymous private 映射，允许区间包含未映射洞。
///
/// @param address page-aligned 起始地址。
/// @param length 非零长度，向上取整到整页。
/// @return 成功返回零；非法范围或触及非 anonymous VMA 返回负 errno。
pub(crate) fn sys_munmap(address: usize, length: usize) -> isize {
    current_task()
        .expect("munmap requires a current task")
        .unmap_anonymous(address, length)
        .map_or_else(|error| -memory_errno(error), |()| 0)
}

/// @description 修改完整 anonymous private 区间的页权限并强制 W^X。
///
/// @param address page-aligned 起始地址。
/// @param length 非零长度，向上取整到整页。
/// @param prot `PROT_READ/WRITE/EXEC` 子集；当前不支持 `PROT_NONE`。
/// @return 成功返回零；缺页、越界或权限策略失败返回负 errno。
pub(crate) fn sys_mprotect(address: usize, length: usize, prot: usize) -> isize {
    let permission = match permission_from_prot(prot) {
        Ok(permission) => permission,
        Err(error) => return -error,
    };
    current_task()
        .expect("mprotect requires a current task")
        .protect_anonymous(address, length, permission)
        .map_or_else(|error| -memory_errno(error), |()| 0)
}
