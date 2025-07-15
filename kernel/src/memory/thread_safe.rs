use alloc::{sync::Arc, collections::BTreeMap, vec::Vec};
use core::sync::atomic::{AtomicUsize, Ordering};
use spin::{Mutex, RwLock};
use crate::{
    sync::UPSafeCell,
    memory::{
        address::{VirtualAddress, PhysicalPageNumber, VirtualPageNumber},
        frame_allocator::{FrameTracker, alloc, alloc_contiguous},
        mm::{MemorySet, MapPermission, MapArea},
        config::{PAGE_SIZE, USER_STACK_SIZE},
    },
    thread::ThreadId,
};

/// 线程内存分配统计
#[derive(Debug, Clone)]
pub struct ThreadMemStats {
    /// 已分配的页面数
    pub allocated_pages: usize,
    /// 用户栈页面数
    pub stack_pages: usize,
    /// 堆页面数
    pub heap_pages: usize,
    /// 总虚拟内存大小（字节）
    pub virtual_memory_size: usize,
}

impl ThreadMemStats {
    pub fn new() -> Self {
        Self {
            allocated_pages: 0,
            stack_pages: 0,
            heap_pages: 0,
            virtual_memory_size: 0,
        }
    }

    pub fn add_pages(&mut self, pages: usize, region_type: MemoryRegionType) {
        self.allocated_pages += pages;
        match region_type {
            MemoryRegionType::Stack => self.stack_pages += pages,
            MemoryRegionType::Heap => self.heap_pages += pages,
            _ => {}
        }
        self.virtual_memory_size += pages * PAGE_SIZE;
    }

    pub fn remove_pages(&mut self, pages: usize, region_type: MemoryRegionType) {
        self.allocated_pages = self.allocated_pages.saturating_sub(pages);
        match region_type {
            MemoryRegionType::Stack => self.stack_pages = self.stack_pages.saturating_sub(pages),
            MemoryRegionType::Heap => self.heap_pages = self.heap_pages.saturating_sub(pages),
            _ => {}
        }
        self.virtual_memory_size = self.virtual_memory_size.saturating_sub(pages * PAGE_SIZE);
    }
}

/// 内存区域类型
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MemoryRegionType {
    Stack,
    Heap,
    Code,
    Data,
    TrapContext,
    Shared,
}

/// 线程内存区域信息
#[derive(Debug)]
pub struct ThreadMemoryRegion {
    /// 虚拟地址范围
    pub start_va: VirtualAddress,
    pub end_va: VirtualAddress,
    /// 权限
    pub permission: MapPermission,
    /// 区域类型
    pub region_type: MemoryRegionType,
    /// 物理页面追踪器
    pub frames: Vec<Arc<FrameTracker>>,
    /// 是否共享
    pub shared: bool,
    /// 引用计数（用于共享内存）
    pub ref_count: AtomicUsize,
}

impl ThreadMemoryRegion {
    pub fn new(
        start_va: VirtualAddress,
        end_va: VirtualAddress,
        permission: MapPermission,
        region_type: MemoryRegionType,
        shared: bool,
    ) -> Self {
        Self {
            start_va,
            end_va,
            permission,
            region_type,
            frames: Vec::new(),
            shared,
            ref_count: AtomicUsize::new(1),
        }
    }

    /// 添加物理页面
    pub fn add_frame(&mut self, frame: Arc<FrameTracker>) {
        self.frames.push(frame);
    }

    /// 获取页面数量
    pub fn page_count(&self) -> usize {
        let start_vpn = VirtualPageNumber::from(self.start_va);
        let end_vpn = VirtualPageNumber::from(self.end_va);
        end_vpn.as_usize() - start_vpn.as_usize()
    }

    /// 增加引用计数
    pub fn inc_ref(&self) -> usize {
        self.ref_count.fetch_add(1, Ordering::SeqCst)
    }

    /// 减少引用计数
    pub fn dec_ref(&self) -> usize {
        self.ref_count.fetch_sub(1, Ordering::SeqCst).saturating_sub(1)
    }

    /// 获取引用计数
    pub fn ref_count(&self) -> usize {
        self.ref_count.load(Ordering::SeqCst)
    }
}

/// 线程安全内存管理器
#[derive(Debug)]
pub struct ThreadSafeMemoryManager {
    /// 每个线程的内存统计
    thread_stats: RwLock<BTreeMap<ThreadId, ThreadMemStats>>,
    /// 线程内存区域
    thread_regions: RwLock<BTreeMap<ThreadId, Vec<Arc<Mutex<ThreadMemoryRegion>>>>>,
    /// 共享内存区域
    shared_regions: RwLock<BTreeMap<usize, Arc<Mutex<ThreadMemoryRegion>>>>,
    /// 内存使用限制
    memory_limits: RwLock<BTreeMap<ThreadId, usize>>,
    /// 全局内存分配计数器
    global_allocated_pages: AtomicUsize,
    /// 最大允许的总内存页数
    max_total_pages: usize,
    /// 共享内存ID分配器
    shared_id_allocator: AtomicUsize,
}

impl ThreadSafeMemoryManager {
    pub fn new(max_total_pages: usize) -> Self {
        Self {
            thread_stats: RwLock::new(BTreeMap::new()),
            thread_regions: RwLock::new(BTreeMap::new()),
            shared_regions: RwLock::new(BTreeMap::new()),
            memory_limits: RwLock::new(BTreeMap::new()),
            global_allocated_pages: AtomicUsize::new(0),
            max_total_pages,
            shared_id_allocator: AtomicUsize::new(1),
        }
    }

    /// 注册线程
    pub fn register_thread(&self, thread_id: ThreadId, memory_limit: Option<usize>) {
        let mut stats = self.thread_stats.write();
        stats.insert(thread_id, ThreadMemStats::new());
        drop(stats);

        let mut regions = self.thread_regions.write();
        regions.insert(thread_id, Vec::new());
        drop(regions);

        if let Some(limit) = memory_limit {
            let mut limits = self.memory_limits.write();
            limits.insert(thread_id, limit);
        }

        debug!("Thread {} registered with memory limit: {:?}", thread_id.0, memory_limit);
    }

    /// 注销线程并清理内存
    pub fn unregister_thread(&self, thread_id: ThreadId) {
        // 清理线程的所有内存区域
        if let Some(regions) = {
            let mut thread_regions = self.thread_regions.write();
            thread_regions.remove(&thread_id)
        } {
            for region in regions {
                let region_guard = region.lock();
                let page_count = region_guard.page_count();
                drop(region_guard);

                // 更新统计信息
                self.global_allocated_pages.fetch_sub(page_count, Ordering::SeqCst);
            }
        }

        // 清理统计信息
        let mut stats = self.thread_stats.write();
        stats.remove(&thread_id);
        drop(stats);

        // 清理内存限制
        let mut limits = self.memory_limits.write();
        limits.remove(&thread_id);
        drop(limits);

        info!("Thread {} unregistered and memory cleaned up", thread_id.0);
    }

    /// 为线程分配内存区域
    pub fn allocate_region(
        &self,
        thread_id: ThreadId,
        start_va: VirtualAddress,
        end_va: VirtualAddress,
        permission: MapPermission,
        region_type: MemoryRegionType,
        shared: bool,
    ) -> Result<Arc<Mutex<ThreadMemoryRegion>>, &'static str> {
        let page_count = {
            let start_vpn = VirtualPageNumber::from(start_va);
            let end_vpn = VirtualPageNumber::from(self.align_up_to_page(end_va));
            end_vpn.as_usize() - start_vpn.as_usize()
        };

        // 检查是否超过线程内存限制
        if let Some(limit) = self.memory_limits.read().get(&thread_id) {
            let current_stats = self.thread_stats.read();
            if let Some(stats) = current_stats.get(&thread_id) {
                if (stats.allocated_pages + page_count) * PAGE_SIZE > *limit {
                    return Err("Thread memory limit exceeded");
                }
            }
        }

        // 检查是否超过全局内存限制
        let current_global = self.global_allocated_pages.load(Ordering::SeqCst);
        if current_global + page_count > self.max_total_pages {
            return Err("Global memory limit exceeded");
        }

        // 创建内存区域
        let mut region = ThreadMemoryRegion::new(
            start_va,
            self.align_up_to_page(end_va),
            permission,
            region_type,
            shared
        );

        // 分配物理页面
        for _ in 0..page_count {
            if let Some(frame) = alloc() {
                region.add_frame(Arc::new(frame));
            } else {
                return Err("Failed to allocate physical frame");
            }
        }

        let region_arc = Arc::new(Mutex::new(region));

        // 更新统计信息
        {
            let mut stats = self.thread_stats.write();
            if let Some(thread_stats) = stats.get_mut(&thread_id) {
                thread_stats.add_pages(page_count, region_type);
            }
        }

        // 添加到线程区域列表
        {
            let mut regions = self.thread_regions.write();
            if let Some(thread_regions) = regions.get_mut(&thread_id) {
                thread_regions.push(region_arc.clone());
            }
        }

        // 如果是共享内存，添加到共享区域列表
        if shared {
            let shared_id = self.shared_id_allocator.fetch_add(1, Ordering::SeqCst);
            let mut shared_regions = self.shared_regions.write();
            shared_regions.insert(shared_id, region_arc.clone());
        }

        // 更新全局分配计数
        self.global_allocated_pages.fetch_add(page_count, Ordering::SeqCst);

        debug!("Allocated {} pages for thread {} (type: {:?}, shared: {})",
               page_count, thread_id.0, region_type, shared);

        Ok(region_arc)
    }

    /// 释放线程的内存区域
    pub fn deallocate_region(
        &self,
        thread_id: ThreadId,
        start_va: VirtualAddress,
    ) -> Result<(), &'static str> {
        let mut regions = self.thread_regions.write();

        if let Some(thread_regions) = regions.get_mut(&thread_id) {
            if let Some(pos) = thread_regions.iter().position(|region| {
                let region_guard = region.lock();
                region_guard.start_va == start_va
            }) {
                let region = thread_regions.remove(pos);
                let region_guard = region.lock();

                let page_count = region_guard.page_count();
                let region_type = region_guard.region_type;
                drop(region_guard);

                // 更新统计信息
                {
                    let mut stats = self.thread_stats.write();
                    if let Some(thread_stats) = stats.get_mut(&thread_id) {
                        thread_stats.remove_pages(page_count, region_type);
                    }
                }

                // 更新全局分配计数
                self.global_allocated_pages.fetch_sub(page_count, Ordering::SeqCst);

                debug!("Deallocated {} pages for thread {} at address {:#x}",
                       page_count, thread_id.0, start_va.as_usize());

                return Ok(());
            }
        }

        Err("Memory region not found")
    }

    /// 分配线程栈
    pub fn allocate_thread_stack(
        &self,
        thread_id: ThreadId,
        stack_size: usize,
    ) -> Result<(VirtualAddress, VirtualAddress), &'static str> {
        let aligned_size = self.align_up_to_page_size(stack_size);

        // 计算栈的虚拟地址（在用户地址空间高端）
        let stack_base = VirtualAddress::from(0x70000000usize + thread_id.0 * 0x10000000);
        let stack_top = VirtualAddress::from(stack_base.as_usize() + aligned_size);

        // 分配栈内存区域
        self.allocate_region(
            thread_id,
            stack_base,
            stack_top,
            MapPermission::R | MapPermission::W | MapPermission::U,
            MemoryRegionType::Stack,
            false, // 线程栈不共享
        )?;

        Ok((stack_base, stack_top))
    }

    /// 分配共享内存
    pub fn allocate_shared_memory(
        &self,
        size: usize,
        permission: MapPermission,
    ) -> Result<usize, &'static str> {
        let aligned_size = self.align_up_to_page_size(size);
        let shared_id = self.shared_id_allocator.fetch_add(1, Ordering::SeqCst);

        // 计算共享内存的虚拟地址
        let shared_base = VirtualAddress::from(0x40000000usize + shared_id * aligned_size);
        let shared_end = VirtualAddress::from(shared_base.as_usize() + aligned_size);

        // 创建共享内存区域
        let mut region = ThreadMemoryRegion::new(
            shared_base,
            shared_end,
            permission,
            MemoryRegionType::Shared,
            true,
        );

        let page_count = aligned_size / PAGE_SIZE;

        // 分配物理页面
        for _ in 0..page_count {
            if let Some(frame) = alloc() {
                region.add_frame(Arc::new(frame));
            } else {
                return Err("Failed to allocate physical frame for shared memory");
            }
        }

        let region_arc = Arc::new(Mutex::new(region));

        // 添加到共享区域列表
        {
            let mut shared_regions = self.shared_regions.write();
            shared_regions.insert(shared_id, region_arc);
        }

        // 更新全局分配计数
        self.global_allocated_pages.fetch_add(page_count, Ordering::SeqCst);

        info!("Allocated shared memory region {} with {} pages", shared_id, page_count);

        Ok(shared_id)
    }

    /// 将共享内存映射到线程地址空间
    pub fn map_shared_memory(
        &self,
        thread_id: ThreadId,
        shared_id: usize,
        thread_va: VirtualAddress,
    ) -> Result<(), &'static str> {
        // 从共享区域列表中获取区域
        let shared_region = {
            let shared_regions = self.shared_regions.read();
            shared_regions.get(&shared_id).cloned()
        };

        if let Some(region_arc) = shared_region {
            let region_guard = region_arc.lock();
            region_guard.inc_ref();
            drop(region_guard);

            // 添加到线程区域列表（但不重复分配物理内存）
            let mut regions = self.thread_regions.write();
            if let Some(thread_regions) = regions.get_mut(&thread_id) {
                thread_regions.push(region_arc);
            }

            debug!("Mapped shared memory {} to thread {} at address {:#x}",
                   shared_id, thread_id.0, thread_va.as_usize());

            Ok(())
        } else {
            Err("Shared memory region not found")
        }
    }

    /// 获取线程内存统计
    pub fn get_thread_stats(&self, thread_id: ThreadId) -> Option<ThreadMemStats> {
        let stats = self.thread_stats.read();
        stats.get(&thread_id).cloned()
    }

    /// 获取全局内存统计
    pub fn get_global_stats(&self) -> (usize, usize, usize) {
        let allocated = self.global_allocated_pages.load(Ordering::SeqCst);
        let shared_count = self.shared_regions.read().len();
        let thread_count = self.thread_stats.read().len();

        (allocated, shared_count, thread_count)
    }

    /// 设置线程内存限制
    pub fn set_thread_memory_limit(&self, thread_id: ThreadId, limit: usize) {
        let mut limits = self.memory_limits.write();
        limits.insert(thread_id, limit);
        debug!("Set memory limit for thread {} to {} bytes", thread_id.0, limit);
    }

    /// 内存碎片整理
    pub fn defragment(&self) -> usize {
        let mut compacted_pages = 0;
        let mut threads_to_compact = Vec::new();

        // 收集需要整理的线程列表
        {
            let regions = self.thread_regions.read();
            for (&thread_id, thread_regions) in regions.iter() {
                let mut thread_memory_regions = Vec::new();
                for region in thread_regions {
                    thread_memory_regions.push(region.clone());
                }
                threads_to_compact.push((thread_id, thread_memory_regions));
            }
        }

        // 对每个线程进行内存整理
        for (thread_id, thread_regions) in threads_to_compact {
            compacted_pages += self.defragment_thread_memory(thread_id, &thread_regions);
        }

        // 整理共享内存
        compacted_pages += self.defragment_shared_memory();

        info!("Memory defragmentation completed, compacted {} pages", compacted_pages);
        compacted_pages
    }

    /// 整理单个线程的内存
    fn defragment_thread_memory(&self, thread_id: ThreadId, regions: &[Arc<Mutex<ThreadMemoryRegion>>]) -> usize {
        let mut compacted = 0;
        let mut regions_to_merge = Vec::new();

        // 按地址排序内存区域
        let mut sorted_regions: Vec<_> = regions.iter()
            .map(|r| (r.lock().start_va, r.clone()))
            .collect();
        sorted_regions.sort_by_key(|(addr, _)| addr.as_usize());

        // 查找相邻的同类型区域
        let mut i = 0;
        while i < sorted_regions.len() - 1 {
            let current_region = sorted_regions[i].1.lock();
            let next_region = sorted_regions[i + 1].1.lock();

            // 检查是否可以合并
            if current_region.end_va == next_region.start_va
                && current_region.permission == next_region.permission
                && current_region.region_type == next_region.region_type
                && !current_region.shared && !next_region.shared {

                // 记录需要合并的区域
                regions_to_merge.push((i, i + 1));
                compacted += 1;

                debug!("Found mergeable regions for thread {}: {:#x}-{:#x} and {:#x}-{:#x}",
                       thread_id.0,
                       current_region.start_va.as_usize(),
                       current_region.end_va.as_usize(),
                       next_region.start_va.as_usize(),
                       next_region.end_va.as_usize());
            }

            drop(current_region);
            drop(next_region);
            i += 1;
        }

        // 执行合并 - 完整实现内存区域合并
        if !regions_to_merge.is_empty() {
            let mut thread_regions = self.thread_regions.write();
            if let Some(thread_region_list) = thread_regions.get_mut(&thread_id) {
                // 反向遍历合并列表，避免索引变化的问题
                for &(idx1, idx2) in regions_to_merge.iter().rev() {
                    if idx1 < sorted_regions.len() && idx2 < sorted_regions.len() {
                        let region1 = &sorted_regions[idx1].1;
                        let region2 = &sorted_regions[idx2].1;
                        
                        // 执行实际的区域合并
                        if self.merge_memory_regions(thread_id, region1.clone(), region2.clone()) {
                            // 从线程区域列表中移除被合并的区域
                            thread_region_list.retain(|r| {
                                let r_ptr = Arc::as_ptr(r);
                                let r1_ptr = Arc::as_ptr(region1);
                                let r2_ptr = Arc::as_ptr(region2);
                                r_ptr != r1_ptr && r_ptr != r2_ptr
                            });
                            
                            // 将合并后的区域（region1）添加回列表
                            thread_region_list.push(region1.clone());
                            
                            debug!("Successfully merged memory regions {} and {} for thread {}", 
                                   idx1, idx2, thread_id.0);
                        }
                    }
                }
            }
        }

        compacted
    }

    /// 合并两个相邻的内存区域
    fn merge_memory_regions(
        &self,
        thread_id: ThreadId,
        region1: Arc<Mutex<ThreadMemoryRegion>>,
        region2: Arc<Mutex<ThreadMemoryRegion>>,
    ) -> bool {
        // 获取两个区域的锁，确保按地址顺序锁定避免死锁
        let (first_region, second_region) = {
            let r1_guard = region1.lock();
            let r2_guard = region2.lock();
            
            if r1_guard.start_va.as_usize() < r2_guard.start_va.as_usize() {
                drop(r1_guard);
                drop(r2_guard);
                (region1.clone(), region2.clone())
            } else {
                drop(r1_guard);
                drop(r2_guard);
                (region2.clone(), region1.clone())
            }
        };

        let mut first_guard = first_region.lock();
        let mut second_guard = second_region.lock();

        // 验证区域可以合并
        if first_guard.end_va != second_guard.start_va
            || first_guard.permission != second_guard.permission
            || first_guard.region_type != second_guard.region_type
            || first_guard.shared || second_guard.shared {
            return false;
        }

        // 执行合并：扩展第一个区域的结束地址
        first_guard.end_va = second_guard.end_va;

        // 合并物理页面
        let mut second_frames = Vec::new();
        core::mem::swap(&mut second_frames, &mut second_guard.frames);
        first_guard.frames.extend(second_frames);

        // 更新统计信息
        let merged_pages = second_guard.page_count();
        {
            let mut stats = self.thread_stats.write();
            if let Some(thread_stats) = stats.get_mut(&thread_id) {
                // 第二个区域的页面数量已经包含在第一个区域中了，所以这里不需要更新页面统计
                debug!("Merged {} pages from region 2 into region 1 for thread {}", 
                       merged_pages, thread_id.0);
            }
        }

        // 清空第二个区域（准备销毁）
        second_guard.frames.clear();

        info!("Successfully merged memory regions for thread {}: {:#x}-{:#x} + {:#x}-{:#x} = {:#x}-{:#x}",
              thread_id.0,
              first_guard.start_va.as_usize(),
              second_guard.start_va.as_usize(),
              second_guard.start_va.as_usize(),
              second_guard.end_va.as_usize(),
              first_guard.start_va.as_usize(),
              first_guard.end_va.as_usize());

        true
    }

    /// 整理共享内存
    fn defragment_shared_memory(&self) -> usize {
        let mut compacted = 0;
        let shared_regions = self.shared_regions.read();

        let mut unused_shared_regions = Vec::new();

        // 查找未使用的共享内存区域
        for (&shared_id, region) in shared_regions.iter() {
            let region_guard = region.lock();
            if region_guard.ref_count() <= 1 {
                unused_shared_regions.push(shared_id);
                compacted += region_guard.page_count();
            }
        }

        drop(shared_regions);

        // 清理未使用的共享内存区域
        if !unused_shared_regions.is_empty() {
            let mut shared_regions = self.shared_regions.write();
            for shared_id in unused_shared_regions {
                if let Some(region) = shared_regions.remove(&shared_id) {
                    let region_guard = region.lock();
                    let page_count = region_guard.page_count();
                    drop(region_guard);

                    self.global_allocated_pages.fetch_sub(page_count, Ordering::SeqCst);
                    debug!("Removed unused shared memory region {}", shared_id);
                }
            }
        }

        compacted
    }

    /// 内存屏障 - 确保内存操作的顺序性
    pub fn memory_barrier(&self) {
        core::sync::atomic::fence(Ordering::SeqCst);
    }

    /// 内存同步屏障 - 用于多线程同步
    pub fn sync_barrier(&self) {
        core::sync::atomic::fence(Ordering::AcqRel);
    }

    // 辅助函数
    fn align_up_to_page(&self, addr: VirtualAddress) -> VirtualAddress {
        let addr_val = addr.as_usize();
        VirtualAddress::from((addr_val + PAGE_SIZE - 1) & !(PAGE_SIZE - 1))
    }

    fn align_up_to_page_size(&self, size: usize) -> usize {
        (size + PAGE_SIZE - 1) & !(PAGE_SIZE - 1)
    }
}

/// 全局线程安全内存管理器实例
static THREAD_SAFE_MEMORY_MANAGER: spin::Once<ThreadSafeMemoryManager> = spin::Once::new();

/// 初始化线程安全内存管理器
pub fn init_thread_safe_memory_manager(max_total_pages: usize) {
    THREAD_SAFE_MEMORY_MANAGER.call_once(|| {
        ThreadSafeMemoryManager::new(max_total_pages)
    });
    info!("Thread-safe memory manager initialized with max {} pages", max_total_pages);
}

/// 获取全局线程安全内存管理器
pub fn get_thread_safe_memory_manager() -> &'static ThreadSafeMemoryManager {
    THREAD_SAFE_MEMORY_MANAGER.get().expect("Thread-safe memory manager not initialized")
}

/// 为当前线程分配栈
pub fn allocate_current_thread_stack(
    thread_id: ThreadId,
    stack_size: usize,
) -> Result<(VirtualAddress, VirtualAddress), &'static str> {
    get_thread_safe_memory_manager().allocate_thread_stack(thread_id, stack_size)
}

/// 注册当前线程
pub fn register_current_thread(thread_id: ThreadId, memory_limit: Option<usize>) {
    get_thread_safe_memory_manager().register_thread(thread_id, memory_limit);
}

/// 注销当前线程
pub fn unregister_current_thread(thread_id: ThreadId) {
    get_thread_safe_memory_manager().unregister_thread(thread_id);
}

/// 内存预分配策略
pub struct MemoryPreallocationPolicy {
    /// 预分配的栈页面数
    pub stack_pages: usize,
    /// 预分配的堆页面数
    pub heap_pages: usize,
    /// 是否启用写时复制
    pub copy_on_write: bool,
}

impl Default for MemoryPreallocationPolicy {
    fn default() -> Self {
        Self {
            stack_pages: USER_STACK_SIZE / PAGE_SIZE,
            heap_pages: 64, // 默认预分配64个页面作为堆
            copy_on_write: true,
        }
    }
}

/// 应用内存预分配策略
pub fn apply_preallocation_policy(
    thread_id: ThreadId,
    policy: &MemoryPreallocationPolicy,
) -> Result<(), &'static str> {
    let manager = get_thread_safe_memory_manager();

    // 预分配栈
    if policy.stack_pages > 0 {
        manager.allocate_thread_stack(thread_id, policy.stack_pages * PAGE_SIZE)?;
    }

    // 预分配堆（如果需要）
    if policy.heap_pages > 0 {
        let heap_size = policy.heap_pages * PAGE_SIZE;
        let heap_base = VirtualAddress::from(0x60000000usize + thread_id.0 * 0x10000000);
        let heap_top = VirtualAddress::from(heap_base.as_usize() + heap_size);

        manager.allocate_region(
            thread_id,
            heap_base,
            heap_top,
            MapPermission::R | MapPermission::W | MapPermission::U,
            MemoryRegionType::Heap,
            false,
        )?;
    }

    debug!("Applied memory preallocation policy for thread {}: stack={} pages, heap={} pages",
           thread_id.0, policy.stack_pages, policy.heap_pages);

    Ok(())
}