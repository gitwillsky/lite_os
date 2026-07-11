use core::sync::atomic;

use crate::{
    memory::{
        address::VirtualAddress,
        mm::{MapArea, MapPermission, MapType},
    },
    syscall::errno::{EINVAL, ENOMEM},
    task::current_task,
};

const USER_HEAP_BASE: usize = 0x4000_0000;
const USER_HEAP_LIMIT: usize = 0x8000_0000;

/// @description 查询或设置当前进程的数据段结尾。
///
/// @param new_brk 新的数据段结尾；为零时查询当前值。
/// @return 当前实现成功时返回新的 break，失败返回负 errno。
pub fn sys_brk(new_brk: usize) -> isize {
    let task = current_task().expect("brk requires a current task");
    let current = task.mm.heap_top.load(atomic::Ordering::Relaxed);

    if current == 0 {
        task.mm
            .heap_base
            .store(USER_HEAP_BASE, atomic::Ordering::Relaxed);
        task.mm
            .heap_top
            .store(USER_HEAP_BASE, atomic::Ordering::Relaxed);
    }

    if new_brk == 0 {
        return task.mm.heap_top.load(atomic::Ordering::Relaxed) as isize;
    }

    let heap_base = task.mm.heap_base.load(atomic::Ordering::Relaxed);
    if new_brk < heap_base {
        return -EINVAL;
    }
    if new_brk > USER_HEAP_LIMIT {
        return -ENOMEM;
    }

    let old_brk = task.mm.heap_top.load(atomic::Ordering::Relaxed);
    if new_brk > old_brk {
        let area = MapArea::new(
            VirtualAddress::from(old_brk),
            VirtualAddress::from(new_brk),
            MapType::Framed,
            MapPermission::R | MapPermission::W | MapPermission::U,
        );
        if task.mm.memory_set.lock().push(area, None).is_err() {
            return -ENOMEM;
        }
    } else if new_brk < old_brk {
        task.mm
            .remove_area_with_start_vpn(VirtualAddress::from(new_brk));
    }

    unsafe {
        core::arch::asm!("sfence.vma");
    }
    task.mm.heap_top.store(new_brk, atomic::Ordering::Relaxed);
    new_brk as isize
}
