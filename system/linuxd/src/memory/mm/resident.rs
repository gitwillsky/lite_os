use alloc::sync::Arc;

use super::MemoryError;
use crate::memory::{
    frame_allocator::FrameTracker,
    page_table::PagePermissions,
    permissions::MapPermission,
    shared_file::{SharedFrame, SharedPage, SharedPagePin},
    swappable_page::{PageMappingRef, PagePin, SwappablePage},
};

/// 一个 MM 映射记录对文件页的持有；磁盘页缓存页据此维护 Linux mapcount（`Mapped`）。
///
/// 建立、fork 复制与释放严格对称。内存文件页的映射由 `PageMappingRef` 计数，其
/// `SharedPage::map_count` 为 None。只需要内容 owner 时用 `page()` 克隆 Arc，不能 clone 本令牌。
#[derive(Debug)]
pub(super) struct FilePageMapping(Arc<dyn SharedPage>);

impl FilePageMapping {
    pub(super) fn new(page: &Arc<dyn SharedPage>) -> Self {
        if let Some(count) = page.map_count() {
            count.map();
        }
        Self(page.clone())
    }

    pub(super) fn page(&self) -> &Arc<dyn SharedPage> {
        &self.0
    }
}

impl core::ops::Deref for FilePageMapping {
    type Target = dyn SharedPage;
    fn deref(&self) -> &Self::Target {
        &*self.0
    }
}

impl Clone for FilePageMapping {
    fn clone(&self) -> Self {
        Self::new(&self.0)
    }
}

impl Drop for FilePageMapping {
    fn drop(&mut self) {
        if let Some(count) = self.0.map_count() {
            count.unmap();
        }
    }
}

/// Private/shared-anonymous mapping metadata; physical content belongs to its canonical owner.
#[derive(Debug, Clone)]
pub(super) struct PrivateResident {
    pub(super) backing: PrivateBacking,
    pub(super) discardable: bool,
    pub(super) dirty: bool,
}

/// Supervisor frames cannot swap; all user content shares the one canonical Page mechanism.
#[derive(Debug, Clone)]
pub(super) enum PrivateBacking {
    Pinned(Arc<FrameTracker>),
    User(PageMappingRef),
    /// 未 COW 的文件页；保活 storage page，不能因只有一个 private alias 而原地放宽写权限。
    File {
        page: FilePageMapping,
        mapping: Option<PageMappingRef>,
    },
}

pub(super) enum PrivatePagePin<'a> {
    Pinned(&'a FrameTracker),
    User(PagePin<'a>),
    Cached(&'a SharedFrame),
}

impl PrivatePagePin<'_> {
    pub(super) fn ppn(&self) -> crate::memory::PhysicalPageNumber {
        match self {
            Self::Pinned(frame) => frame.ppn,
            Self::User(pin) => pin.ppn(),
            Self::Cached(frame) => frame.ppn(),
        }
    }

    pub(super) fn copy_frame(&self) -> Option<FrameTracker> {
        match self {
            Self::Pinned(frame) => crate::memory::frame_allocator::alloc_copy(frame.bytes()),
            Self::User(pin) => pin.copy_frame(),
            Self::Cached(frame) => frame.copy_frame(),
        }
    }
}

impl<'a> From<SharedPagePin<'a>> for PrivatePagePin<'a> {
    fn from(pin: SharedPagePin<'a>) -> Self {
        match pin {
            SharedPagePin::Cached(frame) => Self::Cached(frame),
            SharedPagePin::Swappable(pin) => Self::User(pin),
        }
    }
}

impl PrivateResident {
    pub(super) fn pinned(frame: Arc<FrameTracker>) -> Self {
        Self {
            backing: PrivateBacking::Pinned(frame),
            discardable: false,
            dirty: false,
        }
    }

    pub(super) fn user(frame: FrameTracker) -> Result<Self, MemoryError> {
        let page = SwappablePage::new(SharedFrame::from_frame(frame), true)?;
        let pin = page.pin()?;
        Ok(Self::shared(&page, &pin, true))
    }

    pub(super) fn shared(page: &Arc<SwappablePage>, pin: &PagePin<'_>, private: bool) -> Self {
        Self {
            backing: PrivateBacking::User(page.mapping(pin, private)),
            discardable: false,
            dirty: false,
        }
    }

    /// 私有文件的首次 read/execute 共享内容，保留只读 leaf，直到 write fault 才 COW。
    pub(super) fn file(page: &Arc<dyn SharedPage>, pin: &SharedPagePin<'_>) -> Self {
        Self {
            backing: PrivateBacking::File {
                page: FilePageMapping::new(page),
                mapping: match pin {
                    SharedPagePin::Swappable(pin) => Some(
                        page.swappable()
                            .expect("memory file page owner")
                            .mapping(pin, true),
                    ),
                    SharedPagePin::Cached(_) => None,
                },
            },
            discardable: false,
            dirty: false,
        }
    }

    pub(super) fn mapping(&self) -> Option<&PageMappingRef> {
        match &self.backing {
            PrivateBacking::Pinned(_) => None,
            PrivateBacking::User(mapping) => Some(mapping),
            PrivateBacking::File { mapping, .. } => mapping.as_ref(),
        }
    }

    pub(super) fn mapping_mut(&mut self) -> Option<&mut PageMappingRef> {
        match &mut self.backing {
            PrivateBacking::Pinned(_) => None,
            PrivateBacking::User(mapping) => Some(mapping),
            PrivateBacking::File { mapping, .. } => mapping.as_mut(),
        }
    }

    pub(super) fn mark_installed(&mut self, pin: &PrivatePagePin<'_>) {
        if let PrivatePagePin::User(pin) = pin {
            self.mapping_mut()
                .expect("swappable pin must have a logical mapping")
                .mark_installed(pin);
        }
    }

    pub(super) fn is_file_backed(&self) -> bool {
        matches!(self.backing, PrivateBacking::File { .. })
    }

    pub(super) fn is_installed(&self) -> bool {
        self.mapping().is_none_or(PageMappingRef::is_installed)
    }

    pub(super) fn exclusive(&self) -> bool {
        match &self.backing {
            PrivateBacking::Pinned(frame) => Arc::strong_count(frame) == 1,
            PrivateBacking::User(mapping) => mapping.private_exclusive(),
            PrivateBacking::File { .. } => false,
        }
    }

    pub(super) fn pin_resident(&self) -> Result<Option<PrivatePagePin<'_>>, MemoryError> {
        if let Some(mapping) = self.mapping() {
            return Ok(mapping
                .page()
                .pin_resident()?
                .filter(|pin| mapping.installed_on(pin))
                .map(PrivatePagePin::User));
        }
        match &self.backing {
            PrivateBacking::Pinned(frame) => Ok(Some(PrivatePagePin::Pinned(frame))),
            PrivateBacking::File { page, .. } => Ok(Some(page.pin()?.into())),
            PrivateBacking::User(_) => unreachable!("user backing must have a logical mapping"),
        }
    }

    pub(super) fn pinned_frame(&self) -> &Arc<FrameTracker> {
        match &self.backing {
            PrivateBacking::Pinned(frame) => frame,
            PrivateBacking::User(_) | PrivateBacking::File { .. } => {
                panic!("user frame is not pinned")
            }
        }
    }

    pub(super) fn pinned_frame_mut(&mut self) -> &mut Arc<FrameTracker> {
        match &mut self.backing {
            PrivateBacking::Pinned(frame) => frame,
            PrivateBacking::User(_) | PrivateBacking::File { .. } => {
                panic!("user frame is not pinned")
            }
        }
    }

    /// Initialization keeps MM held through publication; no other mapper can swap the page.
    pub(super) fn initialize(&mut self, offset: usize, input: &[u8]) -> Result<(), MemoryError> {
        match &mut self.backing {
            PrivateBacking::Pinned(frame) => Arc::get_mut(frame)
                .expect("unpublished supervisor frame")
                .bytes_mut()[offset..offset + input.len()]
                .copy_from_slice(input),
            PrivateBacking::User(mapping) => mapping.page().pin()?.write(offset, input),
            PrivateBacking::File { .. } => {
                panic!("file content requires COW before initialization")
            }
        }
        Ok(())
    }

    pub(super) fn release_for_reclaim(self) -> bool {
        match self.backing {
            PrivateBacking::Pinned(_) | PrivateBacking::File { .. } => false,
            PrivateBacking::User(mapping) => mapping.discard(),
        }
    }

    /// Leaf permissions project COW and write tracking from the single mapping record.
    pub(super) fn permissions(
        &self,
        permission: MapPermission,
        private_file: bool,
    ) -> PagePermissions {
        let mut flags: PagePermissions = permission.into();
        if !self.exclusive() || self.discardable || private_file && !self.dirty {
            flags.remove(PagePermissions::WRITE);
        }
        flags
    }
}
