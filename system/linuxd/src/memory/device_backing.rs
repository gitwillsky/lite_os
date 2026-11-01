use alloc::vec::Vec;
use core::{
    ops::Range,
    ptr,
    sync::atomic::{AtomicU32, Ordering},
};

use super::{
    FrameAllocationClass, FrameTracker,
    address::PhysicalPageNumber,
    alloc_contiguous,
    vmstat::{self, PageState},
};

const MAX_EXTENT_PAGES: usize = 64;
/// 一份 backing 的最多物理 extent 数；ATTACH_BACKING 请求按它预留条目空间。
pub(crate) const MAX_EXTENTS: usize = 256;

/// 一份 device backing 在 Linux meminfo 中的页类别；由 backing 的唯一分配者声明一次。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeviceMemoryKind {
    /// 对应 Linux drm_gem_shmem：页计入 `Shmem`；用户映射是 `VM_PFNMAP`，不计 RSS 与 `Mapped`。
    GemShmem,
    /// `vm_insert_page(s)` 插入用户 VMA 的驱动页（ALSA vmalloc ring、io_uring region）：
    /// 映射计入进程 RSS 与 `Mapped`。
    InsertedPages,
    /// 只供驱动/DMA 使用、不映射给用户的内部页（virtio-gpu 内核扫描缓冲）。
    Driver,
}

/// device mapping 与 DMA consumer 共享的精确页数 scatter/gather backing。
pub(crate) struct DeviceBacking {
    extents: Vec<DeviceExtent>,
    pages: usize,
    kind: DeviceMemoryKind,
    // OWNER: InsertedPages 的逐页 Linux mapcount（覆盖该页的用户 VMA 视图数），只由 DeviceArea
    // 生命周期增减；其他类别为空。缺失时 fork/重复 mmap 的同一页会被 `Mapped` 重复计数。
    map_counts: Vec<AtomicU32>,
}

struct DeviceExtent {
    first_page: usize,
    frames: FrameTracker,
}

impl DeviceBacking {
    /// 以不超过 256 KiB 的 buddy extent 事务化分配指定页数。
    ///
    /// # Parameters
    ///
    /// - `pages`: 非零逻辑页数；成功 backing 的物理页数与其精确相等。
    /// - `class`: 本次物理页分配是否可消耗 kernel progress reserve。
    /// - `kind`: 对应 Linux 页类别，决定 meminfo 与用户映射的记账方式。
    ///
    /// # Returns
    ///
    /// 成功返回唯一 backing owner；任一 extent 失败时回收完整 prefix 并返回 None。
    pub(crate) fn try_allocate(
        pages: usize,
        class: FrameAllocationClass,
        kind: DeviceMemoryKind,
    ) -> Option<Self> {
        if pages == 0 {
            return None;
        }
        let mut map_counts = Vec::new();
        if kind == DeviceMemoryKind::InsertedPages {
            map_counts.try_reserve_exact(pages).ok()?;
            map_counts.resize_with(pages, || AtomicU32::new(0));
        }
        let mut extents = Vec::new();
        extents.try_reserve_exact(MAX_EXTENTS).ok()?;
        let mut allocated_pages = 0usize;
        while allocated_pages < pages {
            if extents.len() == MAX_EXTENTS {
                return None;
            }
            let remaining = pages - allocated_pages;
            let mut extent_pages = largest_power_of_two(remaining.min(MAX_EXTENT_PAGES));
            let frames = loop {
                if let Some(frames) = alloc_contiguous(extent_pages, class) {
                    break frames;
                }
                extent_pages /= 2;
                if extent_pages == 0 {
                    return None;
                }
            };
            debug_assert_eq!(frames.pages, extent_pages);
            extents.push(DeviceExtent {
                first_page: allocated_pages,
                frames,
            });
            allocated_pages += extent_pages;
        }
        if kind == DeviceMemoryKind::GemShmem {
            vmstat::add(PageState::Shmem, pages);
        }
        Some(Self {
            extents,
            pages,
            kind,
            map_counts,
        })
    }

    /// 返回分配者声明的 Linux 页类别。
    pub(crate) fn kind(&self) -> DeviceMemoryKind {
        self.kind
    }

    /// 一个用户 VMA 视图开始覆盖 `pages`；InsertedPages 每页首次被映射时计入 `Mapped`。
    pub(in crate::memory) fn map_pages(&self, pages: Range<usize>) {
        for count in self.map_counts.get(pages).unwrap_or_default() {
            if count.fetch_add(1, Ordering::Relaxed) == 0 {
                vmstat::add(PageState::FileMapped, 1);
            }
        }
    }

    /// 一个用户 VMA 视图不再覆盖 `pages`；与 `map_pages` 严格成对。
    pub(in crate::memory) fn unmap_pages(&self, pages: Range<usize>) {
        for count in self.map_counts.get(pages).unwrap_or_default() {
            let previous = count.fetch_sub(1, Ordering::Relaxed);
            debug_assert_ne!(previous, 0, "device page unmapped twice");
            if previous == 1 {
                vmstat::sub(PageState::FileMapped, 1);
            }
        }
    }

    /// 返回 backing 的精确逻辑页数。
    ///
    /// # Returns
    ///
    /// 非零页数，不包含 buddy rounding waste。
    pub(crate) fn pages(&self) -> usize {
        self.pages
    }

    /// 把逻辑 page index 投影到唯一物理页。
    ///
    /// # Parameters
    ///
    /// - `index`: 小于 `pages()` 的逻辑页 index。
    ///
    /// # Returns
    ///
    /// index 有效时返回对应物理页，否则返回 None。
    pub(crate) fn page(&self, index: usize) -> Option<PhysicalPageNumber> {
        if index >= self.pages {
            return None;
        }
        let position = self
            .extents
            .partition_point(|extent| extent.first_page <= index)
            .checked_sub(1)?;
        let extent = &self.extents[position];
        extent
            .frames
            .ppn
            .as_usize()
            .checked_add(index - extent.first_page)
            .map(PhysicalPageNumber::from)
    }

    /// 返回 DMA attach 使用的稳定物理 extent 数。
    ///
    /// # Returns
    ///
    /// 1..=256；backing lifetime 内不变。
    pub(crate) fn extent_count(&self) -> usize {
        self.extents.len()
    }

    /// 按稳定 index 返回一个物理连续 extent。
    ///
    /// # Parameters
    ///
    /// - `index`: 小于 `extent_count()` 的 extent index。
    ///
    /// # Returns
    ///
    /// 有效时返回起始物理页与精确页数，否则返回 None。
    pub(crate) fn extent(&self, index: usize) -> Option<(PhysicalPageNumber, usize)> {
        self.extents
            .get(index)
            .map(|extent| (extent.frames.ppn, extent.frames.pages))
    }

    /// 从逻辑 byte range 复制 device backing 内容到 kernel buffer。
    ///
    /// # Parameters
    ///
    /// - `offset`: backing 内 byte offset。
    /// - `output`: kernel-owned initialized output。
    ///
    /// # Returns
    ///
    /// range 完整有效时成功。
    ///
    /// # Errors
    ///
    /// range 越界或算术溢出返回 unit。
    pub(crate) fn read(&self, offset: usize, output: &mut [u8]) -> Result<(), ()> {
        let capacity = self.pages.checked_mul(super::PAGE_SIZE).ok_or(())?;
        let end = offset.checked_add(output.len()).ok_or(())?;
        if end > capacity {
            return Err(());
        }
        let mut copied = 0usize;
        while copied < output.len() {
            let logical = offset + copied;
            let page_index = logical / super::PAGE_SIZE;
            let page_offset = logical % super::PAGE_SIZE;
            let count = (output.len() - copied).min(super::PAGE_SIZE - page_offset);
            let page = self.page(page_index).ok_or(())?;
            // SAFETY: page() 保证 backing 在 self 生命周期内拥有该 frame；范围限制在单页，
            // user mapping 可并发写入，逐 byte 中间状态与 Linux shared-memory 语义一致。
            unsafe {
                ptr::copy_nonoverlapping(
                    page.as_page_ptr().add(page_offset),
                    output.as_mut_ptr().add(copied),
                    count,
                )
            };
            copied += count;
        }
        Ok(())
    }
}

impl DeviceBacking {
    /// 把 kernel buffer 复制进 backing 的逻辑 byte range。
    ///
    /// # Parameters
    ///
    /// - `offset`: backing 内 byte offset。
    /// - `input`: kernel-owned 源数据。
    ///
    /// # Errors
    ///
    /// range 越界或算术溢出返回 unit。
    pub(crate) fn write(&self, offset: usize, input: &[u8]) -> Result<(), ()> {
        let capacity = self.pages.checked_mul(super::PAGE_SIZE).ok_or(())?;
        let end = offset.checked_add(input.len()).ok_or(())?;
        if end > capacity {
            return Err(());
        }
        let mut copied = 0usize;
        while copied < input.len() {
            let logical = offset + copied;
            let page_offset = logical % super::PAGE_SIZE;
            let count = (input.len() - copied).min(super::PAGE_SIZE - page_offset);
            let page = self.page(logical / super::PAGE_SIZE).ok_or(())?;
            // SAFETY: page() 证明 backing 在 self 生命周期内拥有该 frame；范围限制在单页。user
            // mapping 可并发读写同一页，逐 byte 中间状态与 Linux shared-memory 语义一致。
            unsafe {
                ptr::copy_nonoverlapping(
                    input.as_ptr().add(copied),
                    page.as_page_mut_ptr().add(page_offset),
                    count,
                )
            };
            copied += count;
        }
        Ok(())
    }

    /// 返回 backing 内一个 4 字节对齐 `u32` 的 atomic 视图。
    ///
    /// 与 user mapping 共享的 ring 头（head/tail/flags）必须经 atomic 访问：普通读写会与用户态
    /// 的并发 store 构成 data race，且没有 acquire/release 就无法保证 SQE/CQE 内容先于 tail 可见。
    ///
    /// # Parameters
    ///
    /// - `offset`: 4 字节对齐的 byte offset。
    ///
    /// # Returns
    ///
    /// offset 对齐且 4 字节完整落在 backing 内时返回 atomic；否则 None。
    pub(crate) fn atomic_u32(&self, offset: usize) -> Option<&AtomicU32> {
        if !offset.is_multiple_of(core::mem::align_of::<AtomicU32>()) {
            return None;
        }
        let capacity = self.pages.checked_mul(super::PAGE_SIZE)?;
        if offset.checked_add(4)? > capacity {
            return None;
        }
        let page = self.page(offset / super::PAGE_SIZE)?;
        // SAFETY: 偏移 4 字节对齐且页大小是 4 的倍数，所以这 4 字节在同一页内；frame 由 self 拥有，
        // 返回引用的生命周期受 &self 约束；AtomicU32 与 u32 布局相同，并发访问全部经 atomic。
        Some(unsafe {
            &*page
                .as_page_mut_ptr()
                .add(offset % super::PAGE_SIZE)
                .cast::<AtomicU32>()
        })
    }
}

impl Drop for DeviceBacking {
    fn drop(&mut self) {
        if self.kind == DeviceMemoryKind::GemShmem {
            vmstat::sub(PageState::Shmem, self.pages);
        }
    }
}

impl core::fmt::Debug for DeviceBacking {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("DeviceBacking")
            .field("pages", &self.pages)
            .field("kind", &self.kind)
            .field("extents", &self.extents.len())
            .finish()
    }
}

fn largest_power_of_two(value: usize) -> usize {
    1usize << (usize::BITS - 1 - value.leading_zeros())
}
