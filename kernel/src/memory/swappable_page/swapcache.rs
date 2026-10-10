//! canonical Resident 的唯一 retained-slot capability 与只读 swapcache 总数投影。

use super::{SharedFileId, swap};
use core::sync::atomic::{AtomicUsize, Ordering};

// OWNER: 本模块的 CachedSlot 是唯一计数 capability，不是另一份 backing/slot map。
// 只在 Swapped -> Resident 提交中创建；最后 alias 安装/Drop、换出或 backing Drop 时退休。
// 临时 Slot.clone 和 PagePin 不发放计数 capability；缺失此投影会把真实 SwapCached 永远报零。
// Relaxed 只提供 Linux vmstat 同类的近似独立快照，不承担 backing/bytes publication ordering。
static CACHED_PAGES: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug)]
pub(super) struct CachedSlot {
    slot: swap::SwapSlot,
}
impl CachedSlot {
    /// 已完成读取与 frame preparation，在同一 PageState transaction 发布唯一 cache owner。
    pub(super) fn new(slot: swap::SwapSlot) -> Self {
        let previous = CACHED_PAGES.fetch_add(1, Ordering::Relaxed);
        assert!(previous != usize::MAX, "swapcache page count exhausted");
        Self { slot }
    }
    pub(super) fn clone_slot(&self) -> swap::SwapSlot {
        self.slot.clone()
    }
    pub(super) fn identity(&self) -> SharedFileId {
        self.slot.identity()
    }
}
impl Drop for CachedSlot {
    fn drop(&mut self) {
        assert!(
            CACHED_PAGES.fetch_sub(1, Ordering::Relaxed) != 0,
            "swapcache page released twice"
        );
        // Slot 字段随后 Drop；caller 已将整个 capability 移出 PageState，允许最后 Drop signal。
    }
}

/// Resident+retained slot 的物理页数；O(1)近似投影，不扫描 registry 或取得 MM/PageOp。
pub(crate) fn cached_pages() -> usize {
    CACHED_PAGES.load(Ordering::Relaxed)
}
