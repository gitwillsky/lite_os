use core::sync::atomic;

use crate::memory::{
    PAGE_SIZE,
    address::{VirtualAddress, VirtualPageNumber},
    mm::{MapArea, MapPermission, MapType},
};
use crate::syscall::errno::*;
use crate::task::current_task;

/// 用户程序堆的起始地址（在用户空间的高地址）
const USER_HEAP_BASE: usize = 0x40000000;

/// sys_brk - 调整程序的数据段大小
/// 参数：
/// - new_brk: 新的堆结束地址，如果为0则返回当前堆顶
/// 返回值：
/// - 成功：新的堆顶地址
/// - 失败：-1
pub fn sys_brk(new_brk: usize) -> isize {
    let task = current_task().unwrap();

    // 如果 new_brk 为 0，返回当前堆顶
    if new_brk == 0 {
        // 如果堆还没有初始化，先初始化
        if task.mm.heap_top.load(atomic::Ordering::Relaxed) == 0 {
            task.mm.heap_top.store(USER_HEAP_BASE, atomic::Ordering::Relaxed);
            task.mm.heap_base.store(USER_HEAP_BASE, atomic::Ordering::Relaxed);
        }
        return task.mm.heap_top.load(atomic::Ordering::Relaxed) as isize;
    }

    // 初始化堆（如果是第一次调用）
    if task.mm.heap_top.load(atomic::Ordering::Relaxed) == 0 {
        task.mm.heap_top.store(USER_HEAP_BASE, atomic::Ordering::Relaxed);
        task.mm.heap_base.store(USER_HEAP_BASE, atomic::Ordering::Relaxed);
    }

    // 检查新的堆顶是否小于堆基址
    if new_brk < task.mm.heap_base.load(atomic::Ordering::Relaxed) {
        return -EINVAL;
    }

    // 检查新的堆顶是否会与栈冲突（简单检查）
    if new_brk > 0x80000000 {
        return -ENOMEM;
    }

    if new_brk > task.mm.heap_top.load(atomic::Ordering::Relaxed) {
        // 扩大堆
        let start_va = VirtualAddress::from(task.mm.heap_top.load(atomic::Ordering::Relaxed));
        let end_va = VirtualAddress::from(new_brk);

        let map_area = MapArea::new(
            start_va,
            end_va,
            MapType::Framed,
            MapPermission::R | MapPermission::W | MapPermission::U,
        );

        // 添加到内存集合
        task.mm.memory_set.lock().push(map_area, None);

        // 刷新页表
        unsafe {
            core::arch::asm!("sfence.vma");
        }
    } else if new_brk < task.mm.heap_top.load(atomic::Ordering::Relaxed) {
        // 缩小堆
        let start_va = VirtualAddress::from(new_brk);
        let end_va = VirtualAddress::from(task.mm.heap_top.load(atomic::Ordering::Relaxed));

        // 从内存集合中移除区域
        task.mm.remove_area_with_start_vpn(start_va);

        // 刷新页表
        unsafe {
            core::arch::asm!("sfence.vma");
        }
    }

    task.mm.heap_top.store(new_brk, atomic::Ordering::Relaxed);
    new_brk as isize
}

/// sys_sbrk - 相对调整程序的数据段大小
/// 参数：
/// - increment: 增量大小（字节），可以为负数
/// 返回值：
/// - 成功：调整前的堆顶地址
/// - 失败：-1
pub fn sys_sbrk(increment: isize) -> isize {
    let task = current_task().unwrap();

    // 初始化堆（如果是第一次调用）
    let heap_top = task.mm.heap_top.load(atomic::Ordering::Relaxed);
    let current_brk = if heap_top == 0 {
        USER_HEAP_BASE
    } else {
        heap_top
    };

    if increment == 0 {
        return current_brk as isize;
    }

    // 检查溢出
    let new_brk = if increment > 0 {
        match current_brk.checked_add(increment as usize) {
            Some(addr) => addr,
            None => {
                error!("sys_sbrk: increment overflow");
                return -ENOMEM;
            }
        }
    } else {
        match current_brk.checked_sub((-increment) as usize) {
            Some(addr) => addr,
            None => {
                error!("sys_sbrk: decrement underflow");
                return -EINVAL;
            }
        }
    };

    let result = sys_brk(new_brk);
    if result < 0 {
        return result; // 返回具体的错误码而不是统一的 -1
    }

    current_brk as isize
}

/// sys_mmap - 创建内存映射
/// 参数：
/// - addr: 希望映射的虚拟地址（可以为NULL）
/// - length: 映射长度
/// - prot: 保护标志
/// - flags: 映射标志
/// - fd: 文件描述符（对于匿名映射为-1）
/// - offset: 文件偏移
/// 返回值：
/// - 成功：映射的虚拟地址
/// - 失败：-1
pub fn sys_mmap(
    addr: usize,
    length: usize,
    prot: i32,
    flags: i32,
    fd: i32,
    offset: usize,
) -> isize {
    // 当前只支持匿名映射
    if fd != -1 {
        return -ENOTSUP;
    }

    // 检查长度
    if length == 0 {
        return -EINVAL;
    }

    // 页对齐长度
    let aligned_length = (length + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);

    // 转换保护标志
    let mut permissions = MapPermission::U;
    if prot & PROT_READ != 0 {
        permissions |= MapPermission::R;
    }
    if prot & PROT_WRITE != 0 {
        permissions |= MapPermission::W;
    }
    if prot & PROT_EXEC != 0 {
        permissions |= MapPermission::X;
    }

    let task = current_task().unwrap();

    // 找到合适的虚拟地址
    let start_va = if addr == 0 {
        // 自动分配地址
        find_free_area(&task.mm.memory_set.lock(), aligned_length)
    } else {
        // 使用指定地址
        VirtualAddress::from(addr)
    };

    let end_va = VirtualAddress::from(usize::from(start_va) + aligned_length);

    // 创建映射区域
    let map_area = MapArea::new(start_va, end_va, MapType::Framed, permissions);

    // 添加到内存集合
    task.mm.memory_set.lock().push(map_area, None);

    // 刷新页表
    unsafe {
        core::arch::asm!("sfence.vma");
    }

    usize::from(start_va) as isize
}

/// sys_munmap - 解除内存映射
/// 参数：
/// - addr: 要解除映射的虚拟地址
/// - length: 解除映射的长度
/// 返回值：
/// - 成功：0
/// - 失败：-1
pub fn sys_munmap(addr: usize, length: usize) -> isize {
    if length == 0 {
        return -EINVAL;
    }

    let start_va = VirtualAddress::from(addr);
    let end_va = VirtualAddress::from(addr + length);

    let task = current_task().unwrap();

    // 从内存集合中移除区域
    task.mm.remove_area_with_start_vpn(start_va);

    // 刷新页表
    unsafe {
        core::arch::asm!("sfence.vma");
    }

    0
}

// 内存保护标志常量
pub const PROT_READ: i32 = 1;
pub const PROT_WRITE: i32 = 2;
pub const PROT_EXEC: i32 = 4;

// 映射标志常量
pub const MAP_SHARED: i32 = 1;
pub const MAP_PRIVATE: i32 = 2;
pub const MAP_ANONYMOUS: i32 = 0x20;

/// 在内存集合中找到空闲区域
fn find_free_area(memory_set: &crate::memory::mm::MemorySet, length: usize) -> VirtualAddress {
    // 简单实现：从较高地址开始查找
    let mut current_addr = 0x50000000usize;
    let end_addr = 0x80000000usize;

    while current_addr + length < end_addr {
        let start_vpn = VirtualAddress::from(current_addr).floor();
        let end_vpn = VirtualAddress::from(current_addr + length).ceil();

        // 检查这个区域是否空闲
        let mut is_free = true;
        for vpn in usize::from(start_vpn)..usize::from(end_vpn) {
            if memory_set.translate(VirtualPageNumber::from(vpn)).is_some() {
                is_free = false;
                break;
            }
        }

        if is_free {
            return VirtualAddress::from(current_addr);
        }

        current_addr += PAGE_SIZE;
    }

    // 如果没有找到合适的地址，返回错误地址
    VirtualAddress::from(0)
}
