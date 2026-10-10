use alloc::sync::Arc;

use super::MemoryError;
use crate::memory::{
    frame_allocator::FrameTracker,
    page_table::PagePermissions,
    permissions::MapPermission,
    shared_file::SharedFrame,
    swappable_page::{PageMappingRef, PagePin, SwappablePage},
};

/// Private/shared-anonymous mapping metadata; physical swap content belongs to canonical Page.
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
}

pub(super) enum PrivatePagePin<'a> {
    Pinned(&'a FrameTracker),
    User(PagePin<'a>),
}

impl PrivatePagePin<'_> {
    pub(super) fn ppn(&self) -> crate::memory::PhysicalPageNumber {
        match self {
            Self::Pinned(frame) => frame.ppn,
            Self::User(pin) => pin.ppn(),
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

    pub(super) fn mapping(&self) -> Option<&PageMappingRef> {
        match &self.backing {
            PrivateBacking::Pinned(_) => None,
            PrivateBacking::User(mapping) => Some(mapping),
        }
    }

    pub(super) fn is_installed(&self) -> bool {
        self.mapping().is_none_or(PageMappingRef::is_installed)
    }

    pub(super) fn exclusive(&self) -> bool {
        match &self.backing {
            PrivateBacking::Pinned(frame) => Arc::strong_count(frame) == 1,
            PrivateBacking::User(mapping) => mapping.private_exclusive(),
        }
    }

    pub(super) fn pin_resident(&self) -> Result<Option<PrivatePagePin<'_>>, MemoryError> {
        match &self.backing {
            PrivateBacking::Pinned(frame) => Ok(Some(PrivatePagePin::Pinned(frame))),
            PrivateBacking::User(mapping) => Ok(mapping
                .page()
                .pin_resident()?
                .filter(|pin| mapping.installed_on(pin))
                .map(PrivatePagePin::User)),
        }
    }

    pub(super) fn pinned_frame(&self) -> &Arc<FrameTracker> {
        match &self.backing {
            PrivateBacking::Pinned(frame) => frame,
            PrivateBacking::User(_) => panic!("user frame is not pinned"),
        }
    }

    pub(super) fn pinned_frame_mut(&mut self) -> &mut Arc<FrameTracker> {
        match &mut self.backing {
            PrivateBacking::Pinned(frame) => frame,
            PrivateBacking::User(_) => panic!("user frame is not pinned"),
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
        }
        Ok(())
    }

    pub(super) fn release_for_reclaim(self) -> bool {
        match self.backing {
            PrivateBacking::Pinned(_) => false,
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
