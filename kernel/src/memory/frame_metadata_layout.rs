/// @description frame allocator 从物理区间前缀保留的 metadata 布局。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FrameMetadataLayout {
    /// metadata 独占且不进入 buddy allocator 的物理页数。
    pub(crate) metadata_pages: usize,
    /// 扣除 metadata 后进入 buddy allocator 的物理页数。
    pub(crate) allocatable_pages: usize,
}

/// @description 计算一 byte/frame 状态表所需的自举物理页。
///
/// @param total_pages kernel image 之后、物理内存末端之前的总页数。
/// @param page_size architecture 物理页大小，单位 byte。
/// @return 同时容纳 metadata 与至少一个 allocatable frame 时返回唯一布局。
/// @errors 零容量、零页长、算术溢出或 metadata 吃掉完整区间时返回 None。
pub(crate) fn frame_metadata_layout(
    total_pages: usize,
    page_size: usize,
) -> Option<FrameMetadataLayout> {
    // m 页 metadata 能描述 total_pages - m 个 frame，每 frame 一 byte：
    // m * page_size >= total_pages - m，因此 m = ceil(total_pages / (page_size + 1))。
    let divisor = page_size.checked_add(1)?;
    let metadata_pages = total_pages.checked_add(divisor - 1)?.checked_div(divisor)?;
    let allocatable_pages = total_pages.checked_sub(metadata_pages)?;
    if metadata_pages == 0 || allocatable_pages == 0 {
        return None;
    }
    debug_assert!(
        allocatable_pages
            <= metadata_pages
                .checked_mul(page_size)
                .expect("validated frame metadata layout overflow")
    );
    Some(FrameMetadataLayout {
        metadata_pages,
        allocatable_pages,
    })
}

#[cfg(test)]
mod tests {
    use super::{FrameMetadataLayout, frame_metadata_layout};

    const PAGE_SIZE: usize = 4096;

    #[test]
    fn six_gib_does_not_depend_on_bootstrap_heap_capacity() {
        let total_pages = 6 * 1024 * 1024 * 1024usize / PAGE_SIZE;
        let layout = frame_metadata_layout(total_pages, PAGE_SIZE).unwrap();

        assert_eq!(
            layout,
            FrameMetadataLayout {
                metadata_pages: 384,
                allocatable_pages: total_pages - 384,
            }
        );
        assert!(layout.allocatable_pages <= layout.metadata_pages * PAGE_SIZE);
    }

    #[test]
    fn metadata_layout_handles_small_and_invalid_ranges() {
        assert_eq!(
            frame_metadata_layout(PAGE_SIZE + 1, PAGE_SIZE),
            Some(FrameMetadataLayout {
                metadata_pages: 1,
                allocatable_pages: PAGE_SIZE,
            })
        );
        assert_eq!(frame_metadata_layout(0, PAGE_SIZE), None);
        assert_eq!(frame_metadata_layout(1, PAGE_SIZE), None);
        assert_eq!(frame_metadata_layout(16, 0), None);
    }
}
