pub(crate) const PAGE_SIZE: usize = 4096;

#[derive(Debug, Clone, Copy)]
pub(crate) enum FrameAllocationClass {
    KernelCritical,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct PhysicalAddress(usize);

impl PhysicalAddress {
    pub(crate) fn as_usize(self) -> usize {
        self.0
    }

    pub(crate) fn as_mut_ptr<T>(self) -> *mut T {
        self.0 as *mut T
    }
}

impl From<usize> for PhysicalAddress {
    fn from(value: usize) -> Self {
        Self(value)
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct PhysicalPageNumber(usize);

impl PhysicalPageNumber {
    pub(crate) fn as_usize(self) -> usize {
        self.0
    }
}

#[derive(Debug)]
pub(crate) struct FrameTracker {
    pub(crate) ppn: PhysicalPageNumber,
    allocation: core::ptr::NonNull<u8>,
    layout: std::alloc::Layout,
}

impl Drop for FrameTracker {
    fn drop(&mut self) {
        // SAFETY: `allocation` was created with this exact layout and remains uniquely owned.
        unsafe { std::alloc::dealloc(self.allocation.as_ptr(), self.layout) };
    }
}

pub(crate) fn alloc_contiguous(pages: usize, _class: FrameAllocationClass) -> Option<FrameTracker> {
    let layout =
        std::alloc::Layout::from_size_align(pages.checked_mul(PAGE_SIZE)?, PAGE_SIZE).ok()?;
    // SAFETY: nonzero page count and a validated layout request one uniquely owned host fixture.
    let allocation = core::ptr::NonNull::new(unsafe { std::alloc::alloc_zeroed(layout) })?;
    Some(FrameTracker {
        ppn: PhysicalPageNumber(allocation.as_ptr() as usize / PAGE_SIZE),
        allocation,
        layout,
    })
}

#[path = "../../../kernel/src/memory/mm/shootdown.rs"]
#[allow(dead_code)]
pub(crate) mod shootdown;

#[path = "../../../kernel/src/memory/mm/vma_index_state.rs"]
pub(crate) mod vma_index_state;
