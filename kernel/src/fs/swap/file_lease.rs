//! Filesystem-owned swapfile freeze and immutable allocated extent contract.
use crate::{block::BlockDevice, fs::FileSystemError};
use alloc::{sync::Arc, vec::Vec};

/// Allocated 4KiB page range; current ext4 profile has an exact logical-to-swap-page correspondence.
#[derive(Debug)]
pub(crate) struct SwapExtent {
    pub(in crate::fs) start: usize,
    pub(in crate::fs) end: usize,
    pub(in crate::fs) physical: usize,
}

impl SwapExtent {
    /// Locate the one frozen physical page without allocation or any filesystem lock.
    pub(in crate::fs) fn physical_page(extents: &[Self], page: usize) -> Option<usize> {
        let index = extents.partition_point(|extent| extent.end <= page);
        let extent = extents.get(index).filter(|extent| extent.start <= page)?;
        extent.physical.checked_add(page - extent.start)
    }
}

/// Filesystem-owned immutable layout, exported only after mapped writers and journal are drained.
pub(crate) struct SwapFileLayout {
    pub(in crate::fs) device: Arc<dyn BlockDevice>,
    pub(in crate::fs) extents: Vec<SwapExtent>,
    pub(in crate::fs) size_bytes: u64,
}

/// One canonical inode freeze; failed preparation and successful swapoff use the same infallible close.
pub(crate) trait SwapFileLease: Send + Sync {
    fn activate(&self) -> Result<SwapFileLayout, FileSystemError>;
    fn close(&self);
}
