use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{fmt::Debug, ptr};
use spin::{Mutex, Once};

use super::{address::PhysicalPageNumber, config::PAGE_SIZE, frame_allocator};

/// @description 跨 fs/memory seam 标识一个 mounted filesystem inode。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct SharedFileId {
    pub(crate) filesystem: usize,
    pub(crate) inode: u64,
}

/// @description page-cache 获取共享页时的稳定失败分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SharedFileError {
    OutOfMemory,
    Io,
    BeyondEof,
}

/// @description 可同时被用户页表与 kernel page cache 引用的单页物理存储。
#[derive(Debug)]
pub(crate) struct SharedFrame {
    frame: frame_allocator::FrameTracker,
}

impl SharedFrame {
    /// @description 分配并清零一个共享物理页。
    /// @return 成功返回唯一 owner；物理内存耗尽返回 OutOfMemory。
    pub(crate) fn allocate() -> Result<Self, SharedFileError> {
        frame_allocator::alloc()
            .map(|frame| Self { frame })
            .ok_or(SharedFileError::OutOfMemory)
    }

    /// @description 返回页表映射使用的物理页号。
    pub(crate) fn ppn(&self) -> PhysicalPageNumber {
        self.frame.ppn
    }

    /// @description 从共享页复制到 kernel buffer。
    pub(crate) fn read(&self, offset: usize, output: &mut [u8]) {
        assert!(offset <= PAGE_SIZE && output.len() <= PAGE_SIZE - offset);
        // SAFETY: frame 在 self 生命周期内保持分配；范围已验证。U-mode 可并发修改该页，
        // 与 Linux shared mapping 相同，未同步的并发访问允许观察逐字节的中间结果。
        unsafe {
            ptr::copy_nonoverlapping(
                self.frame.ppn.as_page_ptr().add(offset),
                output.as_mut_ptr(),
                output.len(),
            )
        };
    }

    /// @description 从 kernel buffer 写入共享页并立即对现有 PTE 可见。
    pub(crate) fn write(&self, offset: usize, input: &[u8]) {
        assert!(offset <= PAGE_SIZE && input.len() <= PAGE_SIZE - offset);
        // SAFETY: frame 在 self 生命周期内保持分配；范围已验证。raw physical write 不创建
        // 与硬件 page-table walker 并存的 Rust 引用，用户并发访问遵循 shared-memory 语义。
        unsafe {
            ptr::copy_nonoverlapping(
                input.as_ptr(),
                self.frame.ppn.as_page_mut_ptr().add(offset),
                input.len(),
            )
        };
    }

    /// @description 将页内指定偏移到页尾清零，供 truncate 隐藏旧 EOF 尾部数据。
    pub(crate) fn zero_from(&self, offset: usize) {
        assert!(offset <= PAGE_SIZE);
        // SAFETY: offset 已限制在当前 live frame 内，write_bytes 不越过页尾。
        unsafe {
            ptr::write_bytes(
                self.frame.ppn.as_page_mut_ptr().add(offset),
                0,
                PAGE_SIZE - offset,
            )
        };
    }
}

/// @description MemorySet 持有的共享 cache page interface。
pub(crate) trait SharedPage: Send + Sync + Debug {
    fn frame(&self) -> &SharedFrame;
    fn mark_dirty(&self);
    fn acquire_writer(&self);
    fn release_writer(&self);
}

/// @description file-backed shared VMA 消费的 page-cache interface。
pub(crate) trait SharedFileMapping: Send + Sync + Debug {
    fn id(&self) -> SharedFileId;
    fn size(&self) -> u64;
    fn page(&self, index: u64) -> Result<Arc<dyn SharedPage>, SharedFileError>;
    fn sync_range(&self, offset: u64, length: u64) -> Result<(), SharedFileError>;
}

/// @description memory subsystem 对 live AddressSpace 的反向维护 interface。
pub(crate) trait MemoryMappingOwner: Send + Sync {
    fn invalidate_shared_file(&self, id: SharedFileId, size: u64);
}

/// @description frame allocator 慢路径使用的物理页回收 seam；具体 owner 不泄漏到 memory 下层。
pub(crate) trait MemoryReclaimer: Send + Sync {
    fn reclaim_pages(&self, limit: usize) -> usize;
}

// OWNER: memory module owns the weak address-space invalidation registry. Holding strong Process
// references here would keep exited address spaces and every mapped page alive forever.
static MEMORY_MAPPING_OWNERS: Once<Mutex<Vec<Weak<dyn MemoryMappingOwner>>>> = Once::new();
// OWNER: memory module 只保存 weak reclaimer adapter；强引用会让已退出 AddressSpace 或
// 已移除 page-cache object 永久存活。callback 前释放 registry lock，避免 owner 锁序反转。
static MEMORY_RECLAIMERS: Once<Mutex<Vec<Weak<dyn MemoryReclaimer>>>> = Once::new();

/// @description 注册一个 address-space invalidator，registry 只保留 weak lifetime。
pub(crate) fn register_memory_mapping_owner(
    owner: Arc<dyn MemoryMappingOwner>,
) -> Result<(), SharedFileError> {
    let mut owners = MEMORY_MAPPING_OWNERS
        .call_once(|| Mutex::new(Vec::new()))
        .lock();
    owners
        .try_reserve(1)
        .map_err(|_| SharedFileError::OutOfMemory)?;
    owners.push(Arc::downgrade(&owner));
    Ok(())
}

/// @description 注册一个只保留 weak lifetime 的物理页回收 owner。
pub(crate) fn register_memory_reclaimer(
    owner: Arc<dyn MemoryReclaimer>,
) -> Result<(), SharedFileError> {
    let mut owners = MEMORY_RECLAIMERS
        .call_once(|| Mutex::new(Vec::new()))
        .lock();
    owners
        .try_reserve(1)
        .map_err(|_| SharedFileError::OutOfMemory)?;
    owners.push(Arc::downgrade(&owner));
    Ok(())
}

/// @description 在不持有 registry lock 时通知所有 live address spaces 撤销 EOF 外 PTE。
pub(crate) fn invalidate_shared_file(id: SharedFileId, size: u64) -> Result<(), SharedFileError> {
    let mut owners = MEMORY_MAPPING_OWNERS
        .call_once(|| Mutex::new(Vec::new()))
        .lock();
    owners.retain(|owner| owner.strong_count() != 0);
    let mut live = Vec::new();
    live.try_reserve_exact(owners.len())
        .map_err(|_| SharedFileError::OutOfMemory)?;
    live.extend(owners.iter().filter_map(Weak::upgrade));
    drop(owners);
    for owner in live {
        owner.invalidate_shared_file(id, size);
    }
    Ok(())
}

/// @description 在不持有 registry lock 时请求所有 resident owner 回收可重建物理页。
pub(crate) fn reclaim_pages(limit: usize) -> usize {
    let mut reclaimed = 0;
    let mut cursor = 0;
    loop {
        if reclaimed >= limit {
            break;
        }
        // OOM 慢路径不能为 owner snapshot 再分配 Vec；逐项 clone Weak 并在 callback 前释放锁。
        let owner = {
            let mut owners = MEMORY_RECLAIMERS
                .call_once(|| Mutex::new(Vec::new()))
                .lock();
            owners.retain(|owner| owner.strong_count() != 0);
            let owner = owners.get(cursor).cloned();
            cursor += usize::from(owner.is_some());
            owner
        };
        let Some(owner) = owner.and_then(|owner| owner.upgrade()) else {
            break;
        };
        reclaimed += owner.reclaim_pages(limit - reclaimed);
    }
    reclaimed
}
