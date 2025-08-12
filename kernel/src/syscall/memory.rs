use core::sync::atomic::{self, AtomicUsize, Ordering};
use alloc::{collections::BTreeMap, vec::Vec};
use spin::Mutex;

use crate::memory::{
    PAGE_SIZE,
    address::VirtualAddress,
    mm::{MapArea, MapPermission, MapType},
};
use crate::syscall::errno::*;
use crate::task::current_task;
use crate::memory::{frame_allocator::alloc as alloc_frame, page_table::PTEFlags};

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
        if let Err(_) = task.mm.memory_set.lock().push(map_area, None) {
            return -ENOMEM;
        }

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
                debug!("sys_sbrk: increment overflow");
                return -ENOMEM;
            }
        }
    } else {
        match current_brk.checked_sub((-increment) as usize) {
            Some(addr) => addr,
            None => {
                debug!("sys_sbrk: decrement underflow");
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
        // 自动分配地址（委托给 MemorySet）
        task.mm.memory_set.lock().find_free_area_user(aligned_length)
    } else {
        // 使用指定地址
        VirtualAddress::from(addr)
    };

    let end_va = VirtualAddress::from(usize::from(start_va) + aligned_length);

    // 创建映射区域
    let map_area = MapArea::new(start_va, end_va, MapType::Framed, permissions);

    // 添加到内存集合
    if let Err(_) = task.mm.memory_set.lock().push(map_area, None) {
        return 0; // mmap returns 0 on failure
    }

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

// 原本的 find_free_area 已迁移至 MemorySet::find_free_area_user

// ================= 简化共享内存实现 =================

struct ShmSegment {
    frames: Vec<crate::memory::frame_allocator::FrameTracker>,
    size: usize,
    refcnt: usize,
}

static SHM_REGISTRY: Mutex<BTreeMap<usize, ShmSegment>> = Mutex::new(BTreeMap::new());
static SHM_ID_GEN: AtomicUsize = AtomicUsize::new(1);

fn next_shm_id() -> usize { SHM_ID_GEN.fetch_add(1, Ordering::AcqRel) }

pub fn sys_shm_create(size: usize) -> isize {
    if size == 0 { return -EINVAL; }
    let aligned_len = (size + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
    let page_count = aligned_len / PAGE_SIZE;
    let mut frames: Vec<crate::memory::frame_allocator::FrameTracker> = Vec::with_capacity(page_count);
    for _ in 0..page_count {
        match alloc_frame() {
            Some(f) => frames.push(f),
            None => return -ENOMEM,
        }
    }
    let id = next_shm_id();
    let seg = ShmSegment { frames, size: aligned_len, refcnt: 1 };
    SHM_REGISTRY.lock().insert(id, seg);
    id as isize
}

pub fn sys_shm_map(handle: usize, prot: i32) -> isize {
    let mut reg = SHM_REGISTRY.lock();
    let Some(seg) = reg.get_mut(&handle) else { return -ENOENT; };

    let mut perm = MapPermission::U;
    if prot & PROT_READ != 0 { perm |= MapPermission::R; }
    if prot & PROT_WRITE != 0 { perm |= MapPermission::W; }
    if prot & PROT_EXEC != 0 { perm |= MapPermission::X; }

    let task = current_task().unwrap();
    let length = seg.size;
    let user_base = task.mm.memory_set.lock().find_free_area_user(length);
    if user_base.as_usize() == 0 { return -ENOMEM; }

    let page_count = seg.frames.len();
    let mut ms_guard = task.mm.memory_set.lock();
    let pt = ms_guard.get_page_table_mut();

    // 将共享段的物理帧映射到当前进程
    for i in 0..page_count {
        let va = VirtualAddress::from(user_base.as_usize() + i * PAGE_SIZE);
        let ppn = seg.frames[i].ppn;
        let mut flags = PTEFlags::from_bits(perm.bits()).unwrap_or(PTEFlags::U | PTEFlags::R | PTEFlags::W);
        flags |= PTEFlags::U; // 明确用户可访问
        if (prot & PROT_READ) == 0 { flags.remove(PTEFlags::R); }
        if (prot & PROT_WRITE) == 0 { flags.remove(PTEFlags::W); }
        if (prot & PROT_EXEC) == 0 { flags.remove(PTEFlags::X); }
        if let Err(_) = pt.map(va.into(), ppn.into(), flags) { return -ENOMEM; }
    }

    unsafe { core::arch::asm!("sfence.vma"); }

    // 引用计数 +1
    seg.refcnt += 1;
    user_base.as_usize() as isize
}

pub fn sys_shm_close(handle: usize) -> isize {
    let mut reg = SHM_REGISTRY.lock();
    if let Some(mut seg) = reg.remove(&handle) {
        seg.refcnt = seg.refcnt.saturating_sub(1);
        if seg.refcnt > 0 {
            // 仍被其他进程持有，放回
            reg.insert(handle, seg);
        }
        0
    } else {
        -ENOENT
    }
}
