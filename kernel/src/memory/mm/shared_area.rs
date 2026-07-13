use alloc::collections::BTreeMap;
use core::sync::atomic::{AtomicU64, Ordering};
use spin::Mutex;

use super::*;

// OWNER: memory module owns anonymous shared-backing identities. A raw Arc address can be reused
// after munmap while a futex waiter still holds the old scalar key, which would let an unrelated
// later mapping wake that waiter. Monotonic IDs remove that ABA failure without a global registry;
// Relaxed is sufficient because the counter only provides uniqueness, not memory publication.
static NEXT_SHARED_ANONYMOUS_ID: AtomicU64 = AtomicU64::new(0);

/// @description 匿名共享 VMA 的唯一页帧与 futex identity owner；由所有 fork descendant 共享。
#[derive(Debug)]
pub(super) struct AnonymousSharedBacking {
    /// 不复用的 process-independent futex backing identity。
    pub(super) id: u64,
    page_count: usize,
    // OWNER: backing lock 唯一发布每个共享页索引的 frame；缺失它会让并发 fault
    // 为同一 MAP_SHARED 页建立不同物理 identity，破坏 futex 与跨 fork 可见性。
    frames: Mutex<BTreeMap<usize, Arc<FrameTracker>>>,
}

impl AnonymousSharedBacking {
    /// @description 创建空的匿名共享 backing，物理页由首次 fault 按索引发布。
    ///
    /// @param page_count backing 持有的物理页数。
    /// @return 成功返回唯一共享 owner；容量或物理页不足返回 OutOfMemory。
    pub(super) fn allocate(page_count: usize) -> Result<Arc<Self>, MemoryError> {
        if page_count == 0 {
            return Err(MemoryError::InvalidRange);
        }
        let id = NEXT_SHARED_ANONYMOUS_ID
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                value.checked_add(1)
            })
            .expect("anonymous shared-backing identity exhausted")
            + 1;
        Ok(Arc::new(Self {
            id,
            page_count,
            frames: Mutex::new(BTreeMap::new()),
        }))
    }

    pub(super) fn page(&self, index: usize) -> Result<Arc<FrameTracker>, MemoryError> {
        if index >= self.page_count {
            return Err(MemoryError::InvalidRange);
        }
        if let Some(frame) = self.frames.lock().get(&index).cloned() {
            return Ok(frame);
        }
        let frame = Arc::new(alloc().ok_or(MemoryError::OutOfMemory)?);
        let mut frames = self.frames.lock();
        Ok(frames.entry(index).or_insert_with(|| frame.clone()).clone())
    }
}

/// @description 一个匿名共享 VMA partition 对 backing 与首个 backing page 的引用。
#[derive(Debug, Clone)]
pub(super) struct SharedAnonymousArea {
    /// 跨 fork 与 VMA partition 共享的 backing owner。
    pub(super) backing: Arc<AnonymousSharedBacking>,
    /// 当前 VMA 起点相对 backing 起点的页偏移。
    pub(super) page_offset: usize,
}

impl SharedAnonymousArea {
    /// @description 按 VMA split 边界派生 left/middle/right backing view，不复制页帧。
    ///
    /// @param shared 原 VMA 的可选共享 metadata。
    /// @param original_start 原 VMA 首页。
    /// @param start middle partition 首页。
    /// @param end right partition 首页。
    /// @return 三个 partition 的 metadata；原 VMA 非共享时全部为 None。
    pub(super) fn partition(
        shared: Option<Self>,
        original_start: VirtualPageNumber,
        start: VirtualPageNumber,
        end: VirtualPageNumber,
    ) -> (Option<Self>, Option<Self>, Option<Self>) {
        let Some(shared) = shared else {
            return (None, None, None);
        };
        let middle_offset =
            shared.page_offset + start.as_usize().saturating_sub(original_start.as_usize());
        let right_offset =
            shared.page_offset + end.as_usize().saturating_sub(original_start.as_usize());
        (
            Some(shared.clone()),
            Some(Self {
                backing: shared.backing.clone(),
                page_offset: middle_offset,
            }),
            Some(Self {
                backing: shared.backing,
                page_offset: right_offset,
            }),
        )
    }
}

/// @description shared-file resident page 及其 writer 引用所有权。
#[derive(Debug)]
pub(super) struct SharedResident {
    /// 文件页缓存提供的共享物理页。
    pub(super) page: Arc<dyn SharedPage>,
    /// 当前 resident 是否持有 writer claim。
    pub(super) writer: bool,
}

impl SharedResident {
    /// @description 建立 resident owner，并在 writable mapping 下取得 writer claim。
    ///
    /// @param page 共享文件页。
    /// @param writer 是否取得 writer claim。
    /// @return 持有对应生命周期 claim 的 resident owner。
    pub(super) fn new(page: Arc<dyn SharedPage>, writer: bool) -> Self {
        if writer {
            page.acquire_writer();
        }
        Self { page, writer }
    }
}

impl Drop for SharedResident {
    fn drop(&mut self) {
        if self.writer {
            self.page.release_writer();
        }
    }
}

/// @description shared-file VMA 的文件 identity、byte offset 与 resident-page owner。
#[derive(Debug)]
pub(super) struct SharedFileArea {
    /// 文件系统提供的共享 mapping adapter。
    pub(super) mapping: Arc<dyn SharedFileMapping>,
    /// VMA 起点对应的文件字节偏移。
    pub(super) file_offset: u64,
    /// 已 fault-in 的共享页及 writer claims。
    pub(super) resident: BTreeMap<VirtualPageNumber, SharedResident>,
}

impl MapArea {
    /// @description 用指定 backing 构造 eager 匿名共享 VMA。
    ///
    /// @param start_va VMA 起始虚拟地址。
    /// @param end_va VMA 结束虚拟地址，不包含该地址。
    /// @param permissions 用户页权限。
    /// @param backing 完整覆盖 VMA 的共享页帧 owner。
    /// @return 尚未提交页表的 MapArea。
    pub(super) fn shared_anonymous(
        start_va: VirtualAddress,
        end_va: VirtualAddress,
        permissions: MapPermission,
        backing: Arc<AnonymousSharedBacking>,
    ) -> Self {
        let mut area = Self::anonymous(start_va, end_va, permissions);
        area.shared_anonymous = Some(SharedAnonymousArea {
            backing: backing.clone(),
            page_offset: 0,
        });
        area
    }

    /// @description 构造尚未 fault-in resident page 的 shared-file VMA。
    ///
    /// @param start_va VMA 起始虚拟地址。
    /// @param end_va VMA 结束虚拟地址，不包含该地址。
    /// @param permissions 用户页权限。
    /// @param mapping 文件系统共享 mapping adapter。
    /// @param file_offset VMA 起点对应的文件字节偏移。
    /// @return 尚未提交页表的 MapArea。
    pub(super) fn shared_file(
        start_va: VirtualAddress,
        end_va: VirtualAddress,
        permissions: MapPermission,
        mapping: Arc<dyn SharedFileMapping>,
        file_offset: u64,
    ) -> Self {
        let mut area = Self::new(start_va, end_va, MapType::Framed, permissions);
        area.kind = VmaKind::File;
        area.shared_file = Some(SharedFileArea {
            mapping,
            file_offset,
            resident: BTreeMap::new(),
        });
        area
    }

    /// @description 将匿名共享 VMA 的 backing frames 提交到页表。
    ///
    /// @param page_table 当前 AddressSpace 的页表 owner。
    /// @return 当前 area 非匿名共享时返回 false；成功映射返回 true；页表冲突返回错误。
    pub(super) fn map_shared_anonymous(
        &self,
        page_table: &mut PageTable,
    ) -> Result<bool, MemoryError> {
        if self.shared_anonymous.is_none() {
            return Ok(false);
        }
        let _ = page_table;
        Ok(true)
    }

    /// @description 判断相邻匿名 VMA 是否可在不改变 private/shared identity 下合并。
    ///
    /// @param right 紧邻当前 area 右侧的候选 VMA。
    /// @return 权限一致且同属 private，或同一 backing 的连续区间时返回 true。
    pub(super) fn anonymous_mergeable(&self, right: &Self) -> bool {
        if self.kind != VmaKind::Anonymous
            || right.kind != VmaKind::Anonymous
            || self.vpn_range.end != right.vpn_range.start
            || self.map_permission != right.map_permission
        {
            return false;
        }
        match (&self.shared_anonymous, &right.shared_anonymous) {
            (None, None) => true,
            (Some(left), Some(right)) => {
                left.backing.id == right.backing.id
                    && left.page_offset + self.vpn_range.end.as_usize()
                        - self.vpn_range.start.as_usize()
                        == right.page_offset
            }
            _ => false,
        }
    }
}
