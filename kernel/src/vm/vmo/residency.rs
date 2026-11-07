//! 物理页的所有权记账与映射 backing 观测；不触发 fault 或 pager。
use super::*;

use core::sync::atomic::{AtomicUsize, Ordering};

/// 页框当前保存的内容类别，与对象引用数及映射权限无关。
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(super) enum PageContent {
    Anonymous,
    Pager,
    Bootfs,
    AnonymousPager,
}

// OWNER: Page 是独占页框的 RAII owner，new/Drop 各登记一次；COW 隐藏节点折叠只移动 Page，
// 不重复登记。供页在两棵树锁内转移类别，dirty/pin 在原树锁内转移子计数。计数只用于观测，
// Relaxed 不承载页内容或翻译的同步。缺失时跨服务 pager、COW 祖先和未映射 backing 无法物理去重。
static CONTENT_PAGES: [AtomicUsize; 4] = [const { AtomicUsize::new(0) }; 4];
// OWNER: 仅有存储后备的 pager Page 维护脏页状态子计数；匿名供页没有落盘目标，不进入该计数。
static PAGER_STATES: [AtomicUsize; 2] = [const { AtomicUsize::new(0) }; 2];
// OWNER: 由同一个 Page 的 content/cleanliness/pins 状态转移维护，与实际淘汰判据相同。
static RECLAIMABLE_PAGES: AtomicUsize = AtomicUsize::new(0);
// OWNER: 连续 VMO 在 new_in/Drop 按实际 buddy 分配大小登记；包含不能单独归还的尾部页。
// 不按请求大小统计，否则 DMA 对齐/取整的物理占用无从归属。
static CONTIGUOUS_PAGES: AtomicUsize = AtomicUsize::new(0);

/// VMO 独占物理页的分类；同一页不因 COW、slice 或多次映射而重复。
/// 各字段分别读取，不是跨 CPU 的一致快照；pager 子计数属于 pager_pages，不能再次相加。
pub(crate) struct PhysicalPageStatistics {
    pub(crate) anonymous_pages: usize,
    pub(crate) pager_pages: usize,
    pub(crate) bootfs_pages: usize,
    pub(crate) contiguous_pages: usize,
    pub(crate) pager_dirty_pages: usize,
    pub(crate) pager_writeback_pages: usize,
    pub(crate) pager_reclaimable_pages: usize,
}

/// 读取当前已拥有的物理页分类；不分配、不取树锁、不扫描对象。
pub(crate) fn physical_page_statistics() -> PhysicalPageStatistics {
    PhysicalPageStatistics {
        anonymous_pages: CONTENT_PAGES[PageContent::Anonymous as usize].load(Ordering::Relaxed),
        pager_pages: CONTENT_PAGES[PageContent::Pager as usize].load(Ordering::Relaxed)
            + CONTENT_PAGES[PageContent::AnonymousPager as usize].load(Ordering::Relaxed),
        bootfs_pages: CONTENT_PAGES[PageContent::Bootfs as usize].load(Ordering::Relaxed),
        contiguous_pages: CONTIGUOUS_PAGES.load(Ordering::Relaxed),
        pager_dirty_pages: PAGER_STATES[0].load(Ordering::Relaxed),
        pager_writeback_pages: PAGER_STATES[1].load(Ordering::Relaxed),
        pager_reclaimable_pages: RECLAIMABLE_PAGES.load(Ordering::Relaxed),
    }
}

pub(super) fn add_contiguous(pages: usize) {
    CONTIGUOUS_PAGES.fetch_add(pages, Ordering::Relaxed);
}

pub(super) fn remove_contiguous(pages: usize) {
    CONTIGUOUS_PAGES.fetch_sub(pages, Ordering::Relaxed);
}

impl Page {
    pub(super) fn new(frame: FrameTracker, content: PageContent) -> Self {
        CONTENT_PAGES[content as usize].fetch_add(1, Ordering::Relaxed);
        let page = Self {
            frame,
            pins: 0,
            cleanliness: Cleanliness::Dirty,
            content,
        };
        if content == PageContent::Pager {
            page.pager_state_counter()
                .expect("new page is dirty")
                .fetch_add(1, Ordering::Relaxed);
        }
        page
    }

    pub(super) fn is_reclaimable(&self) -> bool {
        matches!(
            self.content,
            PageContent::Pager | PageContent::AnonymousPager
        ) && self.cleanliness == Cleanliness::Clean
            && self.pins == 0
    }

    /// 调用者持目标与源树锁；只有真正提交到目标的页才改变归属，多余供页直接析构。
    pub(super) fn supply_to_pager(&mut self, content: PageContent) {
        CONTENT_PAGES[self.content as usize].fetch_sub(1, Ordering::Relaxed);
        self.content = content;
        CONTENT_PAGES[self.content as usize].fetch_add(1, Ordering::Relaxed);
        self.cleanliness = Cleanliness::Clean;
        if self.is_reclaimable() {
            RECLAIMABLE_PAGES.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(super) fn set_cleanliness(&mut self, cleanliness: Cleanliness) {
        if self.cleanliness == cleanliness {
            return;
        }
        let was_reclaimable = self.is_reclaimable();
        if let Some(counter) = self.pager_state_counter() {
            counter.fetch_sub(1, Ordering::Relaxed);
        }
        self.cleanliness = cleanliness;
        if let Some(counter) = self.pager_state_counter() {
            counter.fetch_add(1, Ordering::Relaxed);
        }
        self.update_reclaimable(was_reclaimable);
    }

    pub(super) fn set_pins(&mut self, pins: u32) {
        let was_reclaimable = self.is_reclaimable();
        self.pins = pins;
        self.update_reclaimable(was_reclaimable);
    }

    fn pager_state_counter(&self) -> Option<&'static AtomicUsize> {
        if self.content != PageContent::Pager {
            return None;
        }
        match self.cleanliness {
            Cleanliness::Clean => None,
            Cleanliness::Dirty => Some(&PAGER_STATES[0]),
            Cleanliness::AwaitingClean => Some(&PAGER_STATES[1]),
        }
    }

    fn update_reclaimable(&self, was_reclaimable: bool) {
        match (was_reclaimable, self.is_reclaimable()) {
            (true, false) => {
                RECLAIMABLE_PAGES.fetch_sub(1, Ordering::Relaxed);
            }
            (false, true) => {
                RECLAIMABLE_PAGES.fetch_add(1, Ordering::Relaxed);
            }
            _ => {}
        }
    }
}

impl Drop for Page {
    fn drop(&mut self) {
        CONTENT_PAGES[self.content as usize].fetch_sub(1, Ordering::Relaxed);
        if let Some(counter) = self.pager_state_counter() {
            counter.fetch_sub(1, Ordering::Relaxed);
        }
        if self.is_reclaimable() {
            RECLAIMABLE_PAGES.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

impl Vmo {
    pub(super) fn page_content(&self) -> PageContent {
        match self.kind {
            Kind::Paged { content, .. } => content,
            _ => unreachable!("only paged VMOs commit new pages"),
        }
    }

    /// 字节大小。
    pub(crate) fn size(&self) -> u64 {
        (self.state.lock().pages * PAGE_SIZE) as u64
    }
}

impl Vmo {
    /// 本对象自己持有的已提交字节（不含从父对象共享的页）。
    ///
    /// # Errors
    /// 等待者分配失败返回 `NoMemory`。
    pub(crate) fn committed_bytes(&self) -> Result<u64, VmError> {
        let _tree = self.lock_tree()?;
        let state = self.state.lock();
        let pages = match &self.kind {
            Kind::Paged { .. } | Kind::Hidden => state.committed.len(),
            Kind::Contiguous(_) | Kind::Physical { .. } => state.pages,
            Kind::Slice => 0,
        };
        Ok((pages * PAGE_SIZE) as u64)
    }
}
