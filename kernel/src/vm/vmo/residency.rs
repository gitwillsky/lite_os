//! 物理页的所有权记账与映射 backing 观测；不触发 fault 或 pager。
use super::*;

/// 单页唯一 owner；非零标记同时为 `Option<Page>` 提供空槽 niche。
/// 位布局由 Page 的方法封装，树锁串行化 pins/回写/内容归属的变更。
#[repr(transparent)]
pub(super) struct Page(core::num::NonZeroU64);

const _: () = assert!(size_of::<Option<Page>>() == 8);

/// pager 页的回写状态（Zircon page dirty state）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(super) enum Cleanliness {
    /// 内容与 pager 的存储一致，可以淘汰。
    Clean,
    /// 补入后被写过；映射可写。
    Dirty,
    /// 回写已开始（映射已写保护）但存储尚未确认；之后的写入回到 `Dirty`，不能淘汰。
    AwaitingClean,
}

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
    // 44 位页号覆盖受支持后端的完整物理页号；12 位 pin 计数、2 位回写、2 位内容，
    // 最高位为唯一 owner 标记；余下 3 位是环形回收代号，不另增每页字段。
    const PHYSICAL_MASK: u64 = (1 << 44) - 1;
    const PINS_SHIFT: u32 = 44;
    const PINS_MASK: u64 = (1 << 12) - 1;
    const CLEAN_SHIFT: u32 = 56;
    const CONTENT_SHIFT: u32 = 58;
    const GENERATION_SHIFT: u32 = 60;
    const PRESENT: u64 = 1 << 63;

    pub(super) fn ppn(&self) -> PhysicalPageNumber {
        PhysicalPageNumber::from((self.0.get() & Self::PHYSICAL_MASK) as usize)
    }

    pub(super) fn bytes(&self) -> &[u8] {
        // SAFETY: 本槽位保有完整单页的唯一 frame owner；借用不越过 Page，调用者持树锁，
        // 与旧 FrameTracker::bytes 相同，返回只读 direct-map 视图，不创建第二个 frame owner。
        unsafe { core::slice::from_raw_parts(self.ppn().as_page_ptr(), PAGE_SIZE) }
    }

    pub(super) fn pins(&self) -> u32 {
        ((self.0.get() >> Self::PINS_SHIFT) & Self::PINS_MASK) as u32
    }

    pub(super) fn content(&self) -> PageContent {
        match (self.0.get() >> Self::CONTENT_SHIFT) & 3 {
            0 => PageContent::Anonymous,
            1 => PageContent::Pager,
            2 => PageContent::Bootfs,
            _ => PageContent::AnonymousPager,
        }
    }

    pub(super) fn cleanliness(&self) -> Cleanliness {
        match (self.0.get() >> Self::CLEAN_SHIFT) & 3 {
            0 => Cleanliness::Clean,
            1 => Cleanliness::Dirty,
            2 => Cleanliness::AwaitingClean,
            _ => unreachable!("invalid VMO page dirty state"),
        }
    }

    fn set_field(&mut self, shift: u32, mask: u64, value: u64) {
        debug_assert_eq!(value & !mask, 0);
        self.0 = core::num::NonZeroU64::new((self.0.get() & !(mask << shift)) | (value << shift))
            .expect("page owner marker must survive state changes");
    }

    /// 实际 CPU/内核访问进入当前代；fault-around 的相邻预装页不调用此方法。
    pub(super) fn note_access(&mut self) {
        // 匿名/COW 私有页与 bootfs 不参与页回收，不为它们读取全局代号或写 recency metadata。
        if !matches!(
            self.content(),
            PageContent::Pager | PageContent::AnonymousPager
        ) {
            return;
        }
        self.promote_access(super::reclaim::current_generation());
    }

    /// 晋升到调用者已固定的活动代；树锁/扫描协调器保证它不会在事务内被绕回复用。
    pub(super) fn promote_access(&mut self, generation: u8) {
        assert!(generation < 8, "generation exceeds compact page slot");
        self.set_field(Self::GENERATION_SHIFT, 7, u64::from(generation));
    }

    pub(super) fn generation(&self) -> u8 {
        ((self.0.get() >> Self::GENERATION_SHIFT) & 7) as u8
    }

    pub(super) fn new(frame: FrameTracker, content: PageContent) -> Self {
        assert_eq!(frame.pages, 1, "VMO page must own one complete frame");
        let physical = frame.ppn.as_usize() as u64;
        assert_eq!(
            physical & !Self::PHYSICAL_MASK,
            0,
            "physical page exceeds VMO slot capacity"
        );
        CONTENT_PAGES[content as usize].fetch_add(1, Ordering::Relaxed);
        // OWNER: 唯一 frame 所有权移入槽位，直到 Page::drop 才恢复 RAII tracker；
        // 不复制 Page，不提前归还 allocator，也不为纯观测重建第二个 tracker。
        let _frame = core::mem::ManuallyDrop::new(frame);
        let mut page = Self(
            core::num::NonZeroU64::new(
                Self::PRESENT
                    | physical
                    | ((Cleanliness::Dirty as u64) << Self::CLEAN_SHIFT)
                    | ((content as u64) << Self::CONTENT_SHIFT),
            )
            .expect("page owner marker must be nonzero"),
        );
        page.note_access();
        if content == PageContent::Pager {
            page.pager_state_counter()
                .expect("new page is dirty")
                .fetch_add(1, Ordering::Relaxed);
        }
        page
    }

    pub(super) fn is_reclaimable(&self) -> bool {
        matches!(
            self.content(),
            PageContent::Pager | PageContent::AnonymousPager
        ) && self.cleanliness() == Cleanliness::Clean
            && self.pins() == 0
    }

    /// 调用者持目标与源树锁；只有真正提交到目标的页才改变归属，多余供页直接析构。
    pub(super) fn supply_to_pager(&mut self, content: PageContent) {
        CONTENT_PAGES[self.content() as usize].fetch_sub(1, Ordering::Relaxed);
        self.set_field(Self::CONTENT_SHIFT, 3, content as u64);
        CONTENT_PAGES[self.content() as usize].fetch_add(1, Ordering::Relaxed);
        self.set_field(Self::CLEAN_SHIFT, 3, Cleanliness::Clean as u64);
        self.note_access();
        if self.is_reclaimable() {
            RECLAIMABLE_PAGES.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(super) fn set_cleanliness(&mut self, cleanliness: Cleanliness) {
        if self.cleanliness() == cleanliness {
            return;
        }
        let was_reclaimable = self.is_reclaimable();
        if let Some(counter) = self.pager_state_counter() {
            counter.fetch_sub(1, Ordering::Relaxed);
        }
        self.set_field(Self::CLEAN_SHIFT, 3, cleanliness as u64);
        if cleanliness == Cleanliness::Clean {
            self.note_access();
        }
        if let Some(counter) = self.pager_state_counter() {
            counter.fetch_add(1, Ordering::Relaxed);
        }
        self.update_reclaimable(was_reclaimable);
    }

    pub(super) fn set_pins(&mut self, pins: u32) {
        assert!(u64::from(pins) <= Self::PINS_MASK, "VMO pin count overflow");
        let was_reclaimable = self.is_reclaimable();
        self.set_field(Self::PINS_SHIFT, Self::PINS_MASK, u64::from(pins));
        // 固定期间不占代队列；解除最后一个 pin 时重入当前代，旧代可能已经排空并绕回。
        if !was_reclaimable && self.is_reclaimable() {
            self.note_access();
        }
        self.update_reclaimable(was_reclaimable);
    }

    fn pager_state_counter(&self) -> Option<&'static AtomicUsize> {
        if self.content() != PageContent::Pager {
            return None;
        }
        match self.cleanliness() {
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
        CONTENT_PAGES[self.content() as usize].fetch_sub(1, Ordering::Relaxed);
        if let Some(counter) = self.pager_state_counter() {
            counter.fetch_sub(1, Ordering::Relaxed);
        }
        if self.is_reclaimable() {
            RECLAIMABLE_PAGES.fetch_sub(1, Ordering::Relaxed);
        }
        // SAFETY: new 消费的单页 tracker 只由本不可复制的槽位保有；全部映射已按树的
        // zap/fence 契约退休，pin 期间对象不会释放。只在最终析构恢复一次 RAII owner。
        drop(unsafe { FrameTracker::from_raw(self.ppn(), 1) });
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
