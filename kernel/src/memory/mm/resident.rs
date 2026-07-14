use alloc::sync::Arc;
use core::ops::{Deref, DerefMut};

use crate::memory::frame_allocator::FrameTracker;

/// 私有/匿名 VMA 中一个驻留页的完整状态。
///
/// frame、MADV_FREE 与 MAP_PRIVATE dirty 必须同属一个记录；拆成多个树会让一次 page fault
/// 需要多次独立分配，并在中途 OOM 时留下互相矛盾的 residency 状态。
#[derive(Debug, Clone)]
pub(super) struct PrivateResident {
    /// 驻留 frame 的唯一 MemorySet-side lifetime owner。
    pub(super) frame: Arc<FrameTracker>,
    /// MADV_FREE 页在下一次 write fault 前可由 direct reclaim 丢弃。
    pub(super) discardable: bool,
    /// file-backed MAP_PRIVATE 页已写脏，不能再从 immutable backing 重建。
    pub(super) dirty: bool,
}

impl PrivateResident {
    /// 构造尚未被写脏或标记 MADV_FREE 的驻留页。
    ///
    /// @param frame 已在发布前完整初始化的物理页 owner。
    /// @return 与 frame 同时提交的初始 residency 状态。
    pub(super) fn new(frame: Arc<FrameTracker>) -> Self {
        Self {
            frame,
            discardable: false,
            dirty: false,
        }
    }
}

impl Deref for PrivateResident {
    type Target = Arc<FrameTracker>;

    fn deref(&self) -> &Self::Target {
        &self.frame
    }
}

impl DerefMut for PrivateResident {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.frame
    }
}
