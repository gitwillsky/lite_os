use alloc::{collections::BTreeMap, vec::Vec};
use core::sync::atomic::{self, AtomicUsize, Ordering};
use spin::Mutex;

use crate::memory::{
    PAGE_SIZE,
    address::{VirtualAddress, VirtualPageNumber},
    mm::{MapArea, MapPermission, MapType},
    page_table::{PageTableEntry, PTEFlags},
    frame_allocator::alloc as alloc_frame,
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
            task.mm
                .heap_top
                .store(USER_HEAP_BASE, atomic::Ordering::Relaxed);
            task.mm
                .heap_base
                .store(USER_HEAP_BASE, atomic::Ordering::Relaxed);
        }
        return task.mm.heap_top.load(atomic::Ordering::Relaxed) as isize;
    }

    // 初始化堆（如果是第一次调用）
    if task.mm.heap_top.load(atomic::Ordering::Relaxed) == 0 {
        task.mm
            .heap_top
            .store(USER_HEAP_BASE, atomic::Ordering::Relaxed);
        task.mm
            .heap_base
            .store(USER_HEAP_BASE, atomic::Ordering::Relaxed);
    }

    // 检查新的堆顶是否小于堆基址
    if new_brk < task.mm.heap_base.load(atomic::Ordering::Relaxed) {
        return -(EINVAL as isize);
    }

    // 检查新的堆顶是否会与栈冲突（简单检查）
    if new_brk > 0x80000000 {
        return -(ENOMEM as isize);
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
            return -(ENOMEM as isize);
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
                return -(ENOMEM as isize);
            }
        }
    } else {
        match current_brk.checked_sub((-increment) as usize) {
            Some(addr) => addr,
            None => {
                debug!("sys_sbrk: decrement underflow");
                return -(EINVAL as isize);
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
        return -(EINVAL as isize);
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
        task.mm
            .memory_set
            .lock()
            .find_free_area_user(aligned_length)
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
        return -(EINVAL as isize);
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

fn next_shm_id() -> usize {
    SHM_ID_GEN.fetch_add(1, Ordering::AcqRel)
}

pub fn sys_shm_create(size: usize) -> isize {
    if size == 0 {
        return -(EINVAL as isize);
    }
    let aligned_len = (size + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
    let page_count = aligned_len / PAGE_SIZE;
    let mut frames: Vec<crate::memory::frame_allocator::FrameTracker> =
        Vec::with_capacity(page_count);
    for _ in 0..page_count {
        match alloc_frame() {
            Some(f) => frames.push(f),
            None => return -(ENOMEM as isize),
        }
    }
    let id = next_shm_id();
    let seg = ShmSegment {
        frames,
        size: aligned_len,
        refcnt: 1,
    };
    SHM_REGISTRY.lock().insert(id, seg);
    id as isize
}

pub fn sys_shm_map(handle: usize, prot: i32) -> isize {
    let mut reg = SHM_REGISTRY.lock();
    let Some(seg) = reg.get_mut(&handle) else {
        return -ENOENT;
    };

    let mut perm = MapPermission::U;
    if prot & PROT_READ != 0 {
        perm |= MapPermission::R;
    }
    if prot & PROT_WRITE != 0 {
        perm |= MapPermission::W;
    }
    if prot & PROT_EXEC != 0 {
        perm |= MapPermission::X;
    }

    let task = current_task().unwrap();
    let length = seg.size;
    let user_base = task.mm.memory_set.lock().find_free_area_user(length);
    if user_base.as_usize() == 0 {
        return -(ENOMEM as isize);
    }

    let page_count = seg.frames.len();
    let mut ms_guard = task.mm.memory_set.lock();
    let pt = ms_guard.get_page_table_mut();

    // 将共享段的物理帧映射到当前进程
    for i in 0..page_count {
        let va = VirtualAddress::from(user_base.as_usize() + i * PAGE_SIZE);
        let ppn = seg.frames[i].ppn;
        let mut flags =
            PTEFlags::from_bits(perm.bits()).unwrap_or(PTEFlags::U | PTEFlags::R | PTEFlags::W);
        flags |= PTEFlags::U; // 明确用户可访问
        if (prot & PROT_READ) == 0 {
            flags.remove(PTEFlags::R);
        }
        if (prot & PROT_WRITE) == 0 {
            flags.remove(PTEFlags::W);
        }
        if (prot & PROT_EXEC) == 0 {
            flags.remove(PTEFlags::X);
        }
        if let Err(_) = pt.map(va.into(), ppn.into(), flags) {
            return -(ENOMEM as isize);
        }
    }

    unsafe {
        core::arch::asm!("sfence.vma");
    }

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

/// sys_mremap - remap memory region
pub fn sys_mremap(old_addr: usize, old_size: usize, new_size: usize) -> isize {
    use crate::memory::address::VirtualAddress;
    use crate::memory::page_table::translated_byte_buffer;
    
    let task = current_task().unwrap();
    let token = task.mm.user_token();
    
    // Validate parameters
    if old_addr % PAGE_SIZE != 0 || old_size == 0 || new_size == 0 {
        return -(EINVAL as isize);
    }
    
    let old_end = old_addr + old_size;
    let new_end = old_addr + new_size;
    
    // Check if old region is mapped
    let old_start_va = VirtualAddress::from(old_addr);
    let old_end_va = VirtualAddress::from(old_end);
    
    // If new size equals old size, no change needed
    if new_size == old_size {
        return old_addr as isize;
    }
    
    if new_size < old_size {
        // Shrinking - unmap the tail
        let unmap_start = old_addr + new_size;
        let unmap_size = old_size - new_size;
        sys_munmap(unmap_start, unmap_size);
        return old_addr as isize;
    }
    
    // Growing - try to extend in place first
    let extension_start = old_addr + old_size;
    let extension_size = new_size - old_size;
    
    // Check if we can extend in place by trying to map the extension area
    let mut memory_set = task.mm.memory_set.lock();
    
    // Try to extend in place
    let extension_start_va = VirtualAddress::from(extension_start);
    let extension_end_va = VirtualAddress::from(extension_start + extension_size);
    let perm = MapPermission::R | MapPermission::W | MapPermission::U;
    
    match memory_set.insert_framed_area(extension_start_va, extension_end_va, perm) {
        Ok(()) => {
            drop(memory_set);
            
            // Flush TLB
            unsafe {
                core::arch::asm!("sfence.vma");
            }
            
            return old_addr as isize;
        }
        Err(_) => {
            // Cannot extend in place
        }
    }
    
    drop(memory_set);
    
    // Cannot extend in place - allocate new region and copy
    let new_addr = sys_mmap(0, new_size, 3, 0x22, -1, 0); // PROT_READ|WRITE, MAP_PRIVATE|ANON
    if new_addr < 0 {
        return new_addr;
    }
    
    // Copy data from old region to new region
    let copy_size = core::cmp::min(old_size, new_size);
    let old_buffers = translated_byte_buffer(token, old_addr as *const u8, copy_size);
    let new_buffers = translated_byte_buffer(token, new_addr as *const u8, copy_size);
    
    if !old_buffers.is_empty() && !new_buffers.is_empty() {
        let mut copied = 0;
        for (old_buf, new_buf) in old_buffers.iter().zip(new_buffers.iter()) {
            let to_copy = core::cmp::min(old_buf.len(), new_buf.len());
            let to_copy = core::cmp::min(to_copy, copy_size - copied);
            if to_copy == 0 {
                break;
            }
            
            unsafe {
                core::ptr::copy_nonoverlapping(
                    old_buf.as_ptr(),
                    new_buf.as_ptr() as *mut u8,
                    to_copy
                );
            }
            copied += to_copy;
            if copied >= copy_size {
                break;
            }
        }
    }
    
    // Unmap old region
    sys_munmap(old_addr, old_size);
    
    new_addr
}

/// sys_madvise - give advice about use of memory
pub fn sys_madvise(addr: usize, length: usize, advice: i32) -> isize {
    // Validate parameters
    if addr % PAGE_SIZE != 0 || length == 0 {
        return -(EINVAL as isize);
    }
    
    let task = current_task().unwrap();
    let memory_set = task.mm.memory_set.lock();
    
    // Check if the region is mapped
    let start_va = VirtualAddress::from(addr);
    let end_va = VirtualAddress::from(addr + length);
    
    // Check if the memory range is valid by checking if pages are mapped
    let start_vpn = start_va.floor();
    let end_vpn = end_va.ceil();
    
    // Verify at least the start and end pages are mapped
    if memory_set.translate(start_vpn).is_none() {
        return -(ENOMEM as isize);
    }
    
    // Check the last page if range spans multiple pages
    if start_vpn.as_usize() != end_vpn.as_usize() && end_vpn.as_usize() > 0 {
        let last_vpn = VirtualPageNumber::from_vpn(end_vpn.as_usize() - 1);
        if memory_set.translate(last_vpn).is_none() {
            return -(ENOMEM as isize);
        }
    }
    
    // Process advice
    match advice {
        0 => { // MADV_NORMAL
            // Default behavior - no action needed
            0
        },
        1 => { // MADV_RANDOM
            // Expect random access - no prefetching optimization
            0
        },
        2 => { // MADV_SEQUENTIAL
            // Expect sequential access - could optimize for this
            0
        },
        3 => { // MADV_WILLNEED
            // Will need these pages soon - could prefault
            0
        },
        4 => { // MADV_DONTNEED
            // Don't need these pages - could mark for reclaim
            // For now, just return success without actual action
            0
        },
        _ => {
            -(EINVAL as isize)
        }
    }
}

/// mprotect - 修改内存区域的保护属性
pub fn sys_mprotect(addr: usize, length: usize, prot: i32) -> isize {
    if addr % PAGE_SIZE != 0 || length == 0 {
        return -(EINVAL as isize);
    }
    
    let task = match current_task() {
        Some(t) => t,
        None => return -1,
    };
    
    // 检查地址范围是否有效
    let start_va = VirtualAddress::from(addr);
    let end_va = VirtualAddress::from(addr + length);
    let start_vpn = start_va.floor();
    let end_vpn = end_va.ceil();
    
    // 将prot转换为MapPermission
    let mut perm = MapPermission::U;
    if (prot & 1) != 0 { // PROT_READ
        perm |= MapPermission::R;
    }
    if (prot & 2) != 0 { // PROT_WRITE
        perm |= MapPermission::W;
    }
    if (prot & 4) != 0 { // PROT_EXEC
        perm |= MapPermission::X;
    }
    
    // 获取内存集合和页表
    let mut memory_set = task.mm.memory_set.lock();
    
    // 先验证整个范围是否已映射
    let mut current_vpn = start_vpn;
    while current_vpn.as_usize() < end_vpn.as_usize() {
        if memory_set.translate(current_vpn).is_none() {
            return -(ENOMEM as isize);
        }
        current_vpn = current_vpn.next();
    }
    
    // 获取可变的页表引用来修改权限
    let page_table = memory_set.get_page_table_mut();
    
    // 修改页表项权限
    current_vpn = start_vpn;
    while current_vpn.as_usize() < end_vpn.as_usize() {
        if let Some(pte) = page_table.translate(current_vpn) {
            // 创建新的PTE flags
            let mut flags = PTEFlags::V;
            if perm.contains(MapPermission::R) {
                flags |= PTEFlags::R;
            }
            if perm.contains(MapPermission::W) {
                flags |= PTEFlags::W;
            }
            if perm.contains(MapPermission::X) {
                flags |= PTEFlags::X;
            }
            if perm.contains(MapPermission::U) {
                flags |= PTEFlags::U;
            }
            // 保留A和D位
            if pte.flags().contains(PTEFlags::A) {
                flags |= PTEFlags::A;
            }
            if pte.flags().contains(PTEFlags::D) {
                flags |= PTEFlags::D;
            }
            
            // 使用新的update_flags方法更新权限
            if let Err(_) = page_table.update_flags(current_vpn, flags) {
                return -(ENOMEM as isize);
            }
        }
        current_vpn = current_vpn.next();
    }
    
    drop(memory_set);
    
    // 刷新TLB
    unsafe {
        core::arch::asm!("sfence.vma");
    }
    
    0
}

/// msync - 同步内存映射区域到存储设备
pub fn sys_msync(addr: usize, length: usize, flags: i32) -> isize {
    if addr % PAGE_SIZE != 0 || length == 0 {
        return -(EINVAL as isize);
    }
    
    // flags:
    const MS_ASYNC: i32 = 1;      // 异步同步
    const MS_INVALIDATE: i32 = 2; // 使缓存失效  
    const MS_SYNC: i32 = 4;       // 同步写入
    
    // 检查flags有效性
    if (flags & (MS_ASYNC | MS_SYNC)) == 0 {
        return -(EINVAL as isize);
    }
    if (flags & MS_ASYNC) != 0 && (flags & MS_SYNC) != 0 {
        return -(EINVAL as isize);
    }
    
    let task = match current_task() {
        Some(t) => t,
        None => return -1,
    };
    
    // 验证地址范围
    let start_va = VirtualAddress::from(addr);
    let end_va = VirtualAddress::from(addr + length);
    let start_vpn = start_va.floor();
    let end_vpn = end_va.ceil();
    
    let mut memory_set = task.mm.memory_set.lock();
    
    // 遍历页面，同步脏页
    let mut current_vpn = start_vpn;
    while current_vpn.as_usize() < end_vpn.as_usize() {
        if let Some(pte) = memory_set.translate(current_vpn) {
            if !pte.is_valid() {
                return -(ENOMEM as isize);
            }
            
            // 检查脏位
            if pte.flags().contains(PTEFlags::D) {
                // 对于文件映射的页面，获取物理页面数据并写回文件
                let ppn = pte.ppn();
                let page_data = ppn.get_bytes_array_mut();
                
                // 这里应该检查这个页面是否对应文件映射
                // 如果是文件映射，需要将page_data写回到对应的文件偏移处
                // 当前实现主要支持匿名映射，文件映射的写回需要额外的映射信息
                
                // 如果是MS_SYNC，执行同步写入
                if (flags & MS_SYNC) != 0 {
                    // 同步写入：等待所有数据写入完成
                    // 对于文件映射，这里应该调用文件系统的sync方法
                }
                
                // 清除脏位（如果需要）
                if (flags & MS_INVALIDATE) != 0 {
                    let mut new_flags = pte.flags();
                    new_flags.remove(PTEFlags::D);
                    let page_table = memory_set.get_page_table_mut();
                    if let Err(_) = page_table.update_flags(current_vpn, new_flags) {
                        return -(EIO as isize);
                    }
                }
            }
        } else {
            return -(ENOMEM as isize);
        }
        current_vpn = current_vpn.next();
    }
    
    drop(memory_set);
    
    // 如果需要使缓存失效，刷新TLB
    if (flags & MS_INVALIDATE) != 0 {
        unsafe {
            core::arch::asm!("sfence.vma");
        }
    }
    
    0
}

/// 内存锁定状态位图（简单实现）
static LOCKED_PAGES: Mutex<BTreeMap<(usize, VirtualPageNumber), bool>> = Mutex::new(BTreeMap::new());

/// mlock - 锁定内存页面，防止被换出
pub fn sys_mlock(addr: usize, length: usize) -> isize {
    if addr % PAGE_SIZE != 0 || length == 0 {
        return -(EINVAL as isize);
    }
    
    let task = match current_task() {
        Some(t) => t,
        None => return -1,
    };
    
    // 检查权限：通常需要特权或检查锁定限制
    // 简化处理：允许所有用户锁定有限的内存
    const MAX_LOCKED_PAGES: usize = 256; // 最多锁定256页（1MB）
    
    // 验证地址范围
    let start_va = VirtualAddress::from(addr);
    let end_va = VirtualAddress::from(addr + length);
    let start_vpn = start_va.floor();
    let end_vpn = end_va.ceil();
    let pages_to_lock = end_vpn.as_usize() - start_vpn.as_usize();
    
    // 检查是否超过限制
    let pid = task.pid();
    let mut locked_pages = LOCKED_PAGES.lock();
    let current_locked: usize = locked_pages
        .iter()
        .filter(|((p, _), _)| *p == pid)
        .count();
    
    if current_locked + pages_to_lock > MAX_LOCKED_PAGES && !task.is_root() {
        return -(ENOMEM as isize);
    }
    
    // 验证页面是否已映射
    let memory_set = task.mm.memory_set.lock();
    let page_table = memory_set.get_page_table();
    
    let mut current_vpn = start_vpn;
    while current_vpn.as_usize() < end_vpn.as_usize() {
        if page_table.translate(current_vpn).is_none() {
            return -(ENOMEM as isize);
        }
        current_vpn = current_vpn.next();
    }
    
    // 标记页面为锁定
    current_vpn = start_vpn;
    while current_vpn.as_usize() < end_vpn.as_usize() {
        locked_pages.insert((pid, current_vpn), true);
        current_vpn = current_vpn.next();
    }
    
    0
}

/// munlock - 解锁内存页面
pub fn sys_munlock(addr: usize, length: usize) -> isize {
    if addr % PAGE_SIZE != 0 || length == 0 {
        return -(EINVAL as isize);
    }
    
    let task = match current_task() {
        Some(t) => t,
        None => return -1,
    };
    
    // 验证地址范围
    let start_va = VirtualAddress::from(addr);
    let end_va = VirtualAddress::from(addr + length);
    let start_vpn = start_va.floor();
    let end_vpn = end_va.ceil();
    
    let memory_set = task.mm.memory_set.lock();
    let page_table = memory_set.get_page_table();
    
    // 验证页面是否已映射
    let mut current_vpn = start_vpn;
    while current_vpn.as_usize() < end_vpn.as_usize() {
        if page_table.translate(current_vpn).is_none() {
            return -(ENOMEM as isize);
        }
        current_vpn = current_vpn.next();
    }
    
    // 移除锁定标记
    let pid = task.pid();
    let mut locked_pages = LOCKED_PAGES.lock();
    
    current_vpn = start_vpn;
    while current_vpn.as_usize() < end_vpn.as_usize() {
        locked_pages.remove(&(pid, current_vpn));
        current_vpn = current_vpn.next();
    }
    
    0
}

/// mlockall - 锁定进程的所有内存页面
pub fn sys_mlockall(flags: i32) -> isize {
    const MCL_CURRENT: i32 = 1;  // 锁定当前所有映射的页面
    const MCL_FUTURE: i32 = 2;   // 锁定未来映射的页面
    const MCL_ONFAULT: i32 = 4;  // 只在页面错误时锁定
    
    // 验证flags
    if (flags & !(MCL_CURRENT | MCL_FUTURE | MCL_ONFAULT)) != 0 {
        return -(EINVAL as isize);
    }
    
    let task = match current_task() {
        Some(t) => t,
        None => return -1,
    };
    
    // 检查权限限制
    const MAX_LOCKED_PAGES_ROOT: usize = 65536; // 256MB for root
    const MAX_LOCKED_PAGES_USER: usize = 256;   // 1MB for user
    
    let pid = task.pid();
    let is_root = task.is_root();
    let max_pages = if is_root { MAX_LOCKED_PAGES_ROOT } else { MAX_LOCKED_PAGES_USER };
    
    if (flags & MCL_CURRENT) != 0 {
        // 锁定当前所有映射页面
        let memory_set = task.mm.memory_set.lock();
        let mut locked_pages = LOCKED_PAGES.lock();
        
        // 统计当前映射的页面数量
        let mut total_pages = 0;
        for area in memory_set.areas() {
            let page_count = area.vpn_range().end.as_usize() - area.vpn_range().start.as_usize();
            total_pages += page_count;
        }
        
        // 检查是否超过限制
        if total_pages > max_pages {
            return -(ENOMEM as isize);
        }
        
        // 锁定所有映射的页面
        for area in memory_set.areas() {
            let start_vpn = area.vpn_range().start;
            let end_vpn = area.vpn_range().end;
            
            let mut current_vpn = start_vpn;
            while current_vpn.as_usize() < end_vpn.as_usize() {
                // 验证页面确实已映射
                if memory_set.translate(current_vpn).is_some() {
                    locked_pages.insert((pid, current_vpn), true);
                }
                current_vpn = current_vpn.next();
            }
        }
    }
    
    if (flags & MCL_FUTURE) != 0 {
        // 设置进程标志，未来的内存映射都会被锁定
        // 在实际实现中，这需要在进程控制块中保存状态
        // 并在后续的mmap、brk等系统调用中检查此标志
        task.set_mlock_future(true);
    }
    
    0
}

/// munlockall - 解锁进程的所有内存页面
pub fn sys_munlockall() -> isize {
    let task = match current_task() {
        Some(t) => t,
        None => return -1,
    };
    
    let pid = task.pid();
    let mut locked_pages = LOCKED_PAGES.lock();
    
    // 移除该进程的所有锁定页面
    locked_pages.retain(|(p, _), _| *p != pid);
    
    // 清除进程的MLockFuture标志
    task.set_mlock_future(false);
    
    0
}
