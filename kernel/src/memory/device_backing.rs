use alloc::vec::Vec;

use super::{FrameAllocationClass, FrameTracker, address::PhysicalPageNumber, alloc_contiguous};

const MAX_EXTENT_PAGES: usize = 64;
const MAX_EXTENTS: usize = 256;

/// @description device mapping 与 DMA consumer 共享的精确页数 scatter/gather backing。
pub(crate) struct DeviceBacking {
    extents: Vec<DeviceExtent>,
    pages: usize,
}

struct DeviceExtent {
    first_page: usize,
    frames: FrameTracker,
}

impl DeviceBacking {
    /// VirtIO scatter/gather attach 与 allocator transaction 共用的固定 extent 上限。
    pub(crate) const MAX_EXTENTS: usize = MAX_EXTENTS;

    /// @description 以不超过 256 KiB 的 buddy extent 事务化分配指定页数。
    ///
    /// @param pages 非零逻辑页数；成功 backing 的物理页数与其精确相等。
    /// @param class 本次物理页分配是否可消耗 kernel progress reserve。
    /// @return 成功返回唯一 backing owner；任一 extent 失败时回收完整 prefix 并返回 None。
    pub(crate) fn try_allocate(pages: usize, class: FrameAllocationClass) -> Option<Self> {
        if pages == 0 {
            return None;
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
        Some(Self { extents, pages })
    }

    /// @description 返回 backing 的精确逻辑页数。
    /// @return 非零页数，不包含 buddy rounding waste。
    pub(crate) fn pages(&self) -> usize {
        self.pages
    }

    /// @description 把逻辑 page index 投影到唯一物理页。
    ///
    /// @param index 小于 `pages()` 的逻辑页 index。
    /// @return index 有效时返回对应物理页，否则返回 None。
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

    /// @description 返回 DMA attach 使用的稳定物理 extent 数。
    /// @return 1..=256；backing lifetime 内不变。
    pub(crate) fn extent_count(&self) -> usize {
        self.extents.len()
    }

    /// @description 按稳定 index 返回一个物理连续 extent。
    ///
    /// @param index 小于 `extent_count()` 的 extent index。
    /// @return 有效时返回起始物理页与精确页数，否则返回 None。
    pub(crate) fn extent(&self, index: usize) -> Option<(PhysicalPageNumber, usize)> {
        self.extents
            .get(index)
            .map(|extent| (extent.frames.ppn, extent.frames.pages))
    }
}

impl core::fmt::Debug for DeviceBacking {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("DeviceBacking")
            .field("pages", &self.pages)
            .field("extents", &self.extents.len())
            .finish()
    }
}

fn largest_power_of_two(value: usize) -> usize {
    1usize << (usize::BITS - 1 - value.leading_zeros())
}
