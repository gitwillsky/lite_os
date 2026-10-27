//! 用户可换出内容的唯一物理 backing；映射引用和物理迁移在同一短锁中提交。

mod references;
mod registry;
mod swapcache;
use swapcache::CachedSlot;

/// O(1) 的唯一 resident swapcache capability 数量，不扫描 Page/MM。
pub(crate) fn cached_pages() -> usize {
    swapcache::cached_pages()
}

use alloc::sync::{Arc, Weak};
use core::fmt;
use spin::Mutex;

use super::shared_memory::{SharedPageResidency, SharedPageStatistics};
use super::vmstat::{self, PageState};
use super::{SharedFileError, SharedFileId, SharedFrame, address::PhysicalPageNumber, swap};
use crate::sync::{TaskMutex, TaskMutexGuard};
use references::{MappingReference, MappingState};

/// 实际 anonymous shared backing 的 swap admission；仅其页持 Weak，observer 不保活整个 backing。
pub(in crate::memory) trait SharedMemorySwapOwner: Send + Sync {
    /// guard 保持至 swap IO/epoch publication 完成，拒绝锁定或正在切换 policy 的 backing。
    fn try_shared_swap(&self) -> Option<TaskMutexGuard<'_, ()>>;
}

enum Backing {
    Resident {
        frame: Arc<SharedFrame>,
        slot: Option<CachedSlot>,
    },
    Swapped(swap::SwapSlot),
    /// Last private mapping discarded after its fence; temporary candidates own no content.
    Discarded,
}

impl Backing {
    fn residency(&self) -> SharedPageResidency {
        match self {
            Self::Resident { .. } => SharedPageResidency::Resident,
            Self::Swapped(_) => SharedPageResidency::Swapped,
            Self::Discarded => SharedPageResidency::Discarded,
        }
    }
}

struct State {
    backing: Backing,
    mappings: MappingState,
    // OWNER: indexed shared page 的唯一统计 membership；没有额外 resident/swapped 标记。
    // 独立 ledger Arc 不保活 backing，避免 backing→page→backing 环。
    shared_statistics: Option<Arc<SharedPageStatistics>>,
    /// 匿名/COW 私有页为 true；tmpfs、memfd 与 SysV/共享匿名页为 false（Linux shmem）。
    private_origin: bool,
}

impl Drop for State {
    fn drop(&mut self) {
        if let Some(statistics) = &self.shared_statistics {
            statistics.retire(self.backing.residency());
        }
        self.account(self.usage(), (false, false));
    }
}

impl State {
    /// 新建驻留页并计入全局页状态；Drop 对称扣除，创建事务中途失败也不漂移。
    fn new(frame: Arc<SharedFrame>, private_origin: bool) -> Self {
        let state = Self {
            backing: Backing::Resident { frame, slot: None },
            mappings: MappingState::default(),
            shared_statistics: None,
            private_origin,
        };
        state.account((false, false), state.usage());
        state
    }

    /// 本页对全局页状态的贡献：（驻留，至少一个映射已安装到当前物理页）。
    fn usage(&self) -> (bool, bool) {
        let resident = matches!(self.backing, Backing::Resident { .. });
        (resident, resident && self.mappings.installed_count() > 0)
    }

    /// 按 Linux 页类别提交一次贡献变化：私有页只进 `AnonPages`；shmem 驻留进 `Shmem`、映射进 `Mapped`。
    fn account(&self, (was_resident, was_mapped): (bool, bool), (resident, mapped): (bool, bool)) {
        if self.private_origin {
            vmstat::transition(PageState::AnonMapped, was_mapped, mapped);
        } else {
            vmstat::transition(PageState::Shmem, was_resident, resident);
            vmstat::transition(PageState::FileMapped, was_mapped, mapped);
        }
    }

    /// 在同一 PageState 锁内执行映射或 backing 变更，并按前后贡献提交全局页状态。
    fn update<R>(&mut self, change: impl FnOnce(&mut Self) -> R) -> R {
        let before = self.usage();
        let result = change(self);
        self.account(before, self.usage());
        result
    }

    /// 唯一 physical backing replacement；计数与 actual backing 在同一 PageState 事务提交。
    /// 被移出的 frame/slot 仍由调用者在锁外析构。
    fn replace_backing(&mut self, next: Backing) -> Backing {
        if let Some(statistics) = &self.shared_statistics {
            statistics.transition(self.backing.residency(), next.residency());
        }
        core::mem::replace(&mut self.backing, next)
    }

    /// Swapcache 仅为尚未安装的逻辑 SwapEntry 保留槽；驻留内容才是最新内容。
    fn release_unused_slot(&mut self) -> Option<CachedSlot> {
        if self.mappings.stale() == 0
            && let Backing::Resident { slot, .. } = &mut self.backing
        {
            return slot.take();
        }
        None
    }
}

/// 匿名、COW 或内存文件页的单份物理内容；临时 Arc 不参与 COW 计数。
pub(crate) struct SwappablePage {
    /// 真实 anonymous shared backing 的 Weak；不形成 backing→pages→backing 循环。
    shared_owner: Option<Weak<dyn SharedMemorySwapOwner>>,
    // OWNER: blocking IO/physical byte access/迁移由此锁串行。迁移只能 try-MM，
    // 不能等待 MM；fault 持 MM 等本锁，缺失该方向限制会形成跨进程锁环。
    operation: TaskMutex<()>,
    // OWNER: physical backing、epoch 与实际映射计数同锁提交；锁内没有 IO 或最终析构。
    state: Mutex<State>,
}

impl fmt::Debug for SwappablePage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SwappablePage")
            .finish_non_exhaustive()
    }
}

fn access_error(error: swap::SwapError) -> SharedFileError {
    match error {
        swap::SwapError::OutOfMemory => SharedFileError::OutOfMemory,
        _ => SharedFileError::Io,
    }
}

impl SwappablePage {
    /// backing 索引即将无失败发布时取得唯一 membership；不为候选或临时 Arc 收费。
    pub(in crate::memory) fn admit_shared_statistics(&self, statistics: Arc<SharedPageStatistics>) {
        let mut state = self.state.lock();
        assert!(
            state.shared_statistics.is_none(),
            "shared page admitted twice"
        );
        statistics.admit(state.backing.residency());
        state.shared_statistics = Some(statistics);
    }
    /// 发布已填充的页内容，并登记 weak swapoff maintenance slot。
    /// 构造期不登记回收候选；ELF 的 MM 尚未发布，不能扫描其未登记的叶。
    pub(in crate::memory) fn new(
        frame: SharedFrame,
        private_origin: bool,
    ) -> Result<Arc<Self>, SharedFileError> {
        Self::new_with_shared_owner(frame, private_origin, None)
    }
    fn new_with_shared_owner(
        frame: SharedFrame,
        private_origin: bool,
        shared_owner: Option<Weak<dyn SharedMemorySwapOwner>>,
    ) -> Result<Arc<Self>, SharedFileError> {
        let frame = Arc::try_new(frame).map_err(|_| SharedFileError::OutOfMemory)?;
        let page = Arc::try_new(Self {
            shared_owner,
            operation: TaskMutex::new(()),
            state: Mutex::new(State::new(frame, private_origin)),
        })
        .map_err(|_| SharedFileError::OutOfMemory)?;
        registry::register(&page)?;
        Ok(page)
    }

    /// 创建内存文件/共享匿名 backing 的零页；配额仍由 storage owner 持有。
    pub(crate) fn allocate_shared() -> Result<Arc<Self>, SharedFileError> {
        Self::new(SharedFrame::allocate()?, false)
    }
    pub(in crate::memory) fn allocate_anonymous_shared(
        owner: Weak<dyn SharedMemorySwapOwner>,
    ) -> Result<Arc<Self>, SharedFileError> {
        Self::new_with_shared_owner(SharedFrame::allocate()?, false, Some(owner))
    }

    /// 在等待 PageOp 后取得最新物理页；换入成功前不改变旧 backing。
    pub(crate) fn pin(&self) -> Result<PagePin<'_>, SharedFileError> {
        let operation = self
            .operation
            .lock()
            .map_err(|_| SharedFileError::OutOfMemory)?;
        self.pin_locked(operation)
    }

    fn pin_locked<'a>(
        &'a self,
        operation: TaskMutexGuard<'a, ()>,
    ) -> Result<PagePin<'a>, SharedFileError> {
        let slot = {
            let state = self.state.lock();
            match &state.backing {
                Backing::Resident { frame, .. } => {
                    return Ok(PagePin {
                        frame: frame.clone(),
                        _operation: operation,
                        page: self,
                        epoch: state.mappings.epoch(),
                    });
                }
                Backing::Swapped(slot) => slot.clone(),
                Backing::Discarded => return Err(SharedFileError::BeyondEof),
            }
        };
        let mut frame = SharedFrame::allocate()?;
        slot.read(frame.bytes_mut().try_into().expect("swap page size"))
            .map_err(access_error)?;
        let frame = Arc::try_new(frame).map_err(|_| SharedFileError::OutOfMemory)?;
        let (retired, epoch) = {
            let mut state = self.state.lock();
            let retired = state.update(|state| {
                state.replace_backing(Backing::Resident {
                    frame: frame.clone(),
                    slot: Some(CachedSlot::new(slot)),
                })
            });
            let unused = state.release_unused_slot();
            ((retired, unused), state.mappings.epoch())
        };
        // Slot 最终 Drop 可能 signal swapoff；必须在 PageState lock 外。
        drop(retired);
        Ok(PagePin {
            frame,
            _operation: operation,
            page: self,
            epoch,
        })
    }

    /// mprotect/fork 只为仍为 logical Resident 的记录取物理 pin；不主动换入 SwapEntry。
    pub(in crate::memory) fn pin_resident(&self) -> Result<Option<PagePin<'_>>, SharedFileError> {
        let operation = self
            .operation
            .lock()
            .map_err(|_| SharedFileError::OutOfMemory)?;
        let state = self.state.lock();
        Ok(match &state.backing {
            Backing::Resident { frame, .. } => Some(PagePin {
                frame: frame.clone(),
                _operation: operation,
                page: self,
                epoch: state.mappings.epoch(),
            }),
            Backing::Swapped(_) | Backing::Discarded => None,
        })
    }

    /// 已 pin 的新映射的生命周期引用；caller 保持 MM 至 leaf publication 完成。
    pub(in crate::memory) fn mapping(
        self: &Arc<Self>,
        pin: &PagePin<'_>,
        private: bool,
    ) -> PageMappingRef {
        assert!(
            core::ptr::eq(self.as_ref(), pin.page),
            "page pin belongs to another page"
        );
        let reference = self
            .state
            .lock()
            .update(|state| state.mappings.installed(private, pin.epoch));
        PageMappingRef {
            page: self.clone(),
            reference: Some(reference),
        }
    }

    /// 后台执行体迁移单页；所有 MM callbacks 都是不等待的 try-lock。
    /// 取消时保留物理 Resident 和逻辑 epoch，fault 可恢复已撤销的 leaf。
    pub(crate) fn swap_out(&self) -> bool {
        let Some(_operation) = self.operation.try_lock() else {
            return false;
        };
        // 所有 swap 入口（backing/MM/直接候选）都消费同一 backing policy，不只跳过一个 reclaimer。
        // gate 持到 IO/epoch publication 完成，SHM_LOCK 返回后不会迟到提交之前开始的 swap。
        let shared_owner = match &self.shared_owner {
            Some(owner) => match owner.upgrade() {
                Some(owner) => Some(owner),
                None => return false,
            },
            None => None,
        };
        let _shared_swap = match &shared_owner {
            Some(owner) => match owner.try_shared_swap() {
                Some(guard) => Some(guard),
                None => return false,
            },
            None => None,
        };
        let (frame, retained, snapshot) = {
            let state = self.state.lock();
            let Backing::Resident { frame, slot } = &state.backing else {
                return false;
            };
            (
                frame.clone(),
                slot.as_ref().map(CachedSlot::clone_slot),
                state.mappings.snapshot(),
            )
        };
        let slot = match retained {
            // Draining 的 swapcache 等待 swapoff unuse，不以旧槽再次换出。
            Some(slot) if slot.active() => slot,
            Some(_) => return false,
            None => match swap::reserve() {
                Some(slot) => slot,
                None => return false,
            },
        };
        let Some(fenced) = super::shared_file::revoke_swappable_page(self, frame.ppn()) else {
            return false;
        };
        if !self.state.lock().mappings.covered(snapshot, fenced) {
            return false;
        }
        let mut bytes = [0; super::PAGE_SIZE];
        frame.read(0, &mut bytes);
        if slot.write(&bytes).is_err() {
            return false;
        }
        let retired = {
            let mut state = self.state.lock();
            // IO 期间 reference Drop/fork publication 会使证据失效；同锁验证与物理发布。
            let committed = state.update(|state| {
                state
                    .mappings
                    .commit_swap(snapshot, fenced)
                    .then(|| state.replace_backing(Backing::Swapped(slot)))
            });
            let Some(retired) = committed else {
                return false;
            };
            retired
        };
        drop(retired);
        drop(frame);
        true
    }

    fn storage(&self) -> Option<SharedFileId> {
        let state = self.state.lock();
        match &state.backing {
            Backing::Resident { slot, .. } => slot.as_ref().map(CachedSlot::identity),
            Backing::Swapped(slot) => Some(slot.identity()),
            Backing::Discarded => None,
        }
    }
}

/// 持有 PageOp 的物理 pin。frame 字段先析构，operation 最后释放，不能跨 pin 保存旧 PPN。
pub(crate) struct PagePin<'a> {
    frame: Arc<SharedFrame>,
    _operation: TaskMutexGuard<'a, ()>,
    page: &'a SwappablePage,
    epoch: u64,
}

impl PagePin<'_> {
    pub(in crate::memory) fn copy_frame(&self) -> Option<super::frame_allocator::FrameTracker> {
        self.frame.copy_frame()
    }
    pub(crate) fn ppn(&self) -> PhysicalPageNumber {
        self.frame.ppn()
    }
    pub(crate) fn read(&self, offset: usize, output: &mut [u8]) {
        self.frame.read(offset, output);
    }
    pub(crate) fn write(&self, offset: usize, input: &[u8]) {
        self.frame.write(offset, input);
    }
    pub(crate) fn zero_from(&self, offset: usize) {
        self.frame.zero_from(offset);
    }
}

/// 唯一 MM-side logical reference；必须在 hardware retirement fence 后 Drop。
#[derive(Debug)]
pub(in crate::memory) struct PageMappingRef {
    page: Arc<SwappablePage>,
    reference: Option<MappingReference>,
}

impl PageMappingRef {
    /// Direct reclaim consumes this already-fenced mapping. Clear private content under PageOp
    /// when it is the last reference, so temporary candidate Arcs cannot inflate pgsteal.
    pub(in crate::memory) fn discard(mut self) -> bool {
        let Some(_operation) = self.page.operation.try_lock() else {
            return false;
        };
        let (retired, unused_slot) = {
            let mut state = self.page.state.lock();
            let reference = self.reference.take().expect("live reference");
            let (retired, unused_slot) = state.update(|state| {
                state.mappings.release(reference);
                let unused_slot = state.release_unused_slot();
                let retired = (state.private_origin && state.mappings.snapshot().total() == 0)
                    .then(|| state.replace_backing(Backing::Discarded));
                (retired, unused_slot)
            });
            (retired, unused_slot)
        };
        let reclaimed = matches!(retired, Some(Backing::Resident { .. }));
        drop(retired);
        drop(unused_slot);
        reclaimed
    }
    pub(in crate::memory) fn page(&self) -> &Arc<SwappablePage> {
        &self.page
    }
    pub(in crate::memory) fn is_installed(&self) -> bool {
        self.page
            .state
            .lock()
            .mappings
            .is_installed(self.reference.as_ref().expect("live reference"))
    }
    pub(in crate::memory) fn private_exclusive(&self) -> bool {
        let state = self.page.state.lock();
        state.private_origin
            && state
                .mappings
                .private_exclusive(self.reference.as_ref().expect("live reference"))
    }
    pub(in crate::memory) fn matches(&self, page: &SwappablePage) -> bool {
        core::ptr::eq(self.page.as_ref(), page)
    }
    pub(in crate::memory) fn installed_on(&self, pin: &PagePin<'_>) -> bool {
        assert!(self.matches(pin.page), "page pin belongs to another page");
        let state = self.page.state.lock();
        assert_eq!(state.mappings.epoch(), pin.epoch);
        state
            .mappings
            .is_installed(self.reference.as_ref().expect("live reference"))
    }
    /// 成功 leaf/PROT_NONE logical restore 之后提交；不会在 PageState lock 内 Drop slot。
    pub(in crate::memory) fn mark_installed(&mut self, pin: &PagePin<'_>) {
        assert!(self.matches(pin.page), "page pin belongs to another page");
        let retired = {
            let mut state = self.page.state.lock();
            let reference = self.reference.as_mut().expect("live reference");
            state.update(|state| state.mappings.mark_installed(reference, pin.epoch));
            state.release_unused_slot()
        };
        drop(retired);
    }
    pub(in crate::memory) fn storage(&self) -> Option<SharedFileId> {
        self.page.storage()
    }
}

impl Clone for PageMappingRef {
    fn clone(&self) -> Self {
        let parent = self.reference.as_ref().expect("live reference");
        let reference = self
            .page
            .state
            .lock()
            .update(|state| state.mappings.inherit(parent));
        Self {
            page: self.page.clone(),
            reference: Some(reference),
        }
    }
}

impl Drop for PageMappingRef {
    fn drop(&mut self) {
        let Some(reference) = self.reference.take() else {
            return;
        };
        let retired = {
            let mut state = self.page.state.lock();
            state.update(|state| state.mappings.release(reference));
            state.release_unused_slot()
        };
        drop(retired);
    }
}

/// Swapoff 首先恢复 physical backing（含 unmapped storage），且不持 PageOp 等待 MM。
pub(in crate::memory) fn restore_storage<E: From<SharedFileError>>(
    identity: SharedFileId,
    check: &mut impl FnMut() -> Result<(), E>,
) -> Result<(), E> {
    registry::restore(identity, check)
}
