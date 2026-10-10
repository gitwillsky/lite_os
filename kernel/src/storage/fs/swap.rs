//! One immutable page-to-device extent adapter for both partition and regular-file swap.

use super::{BlockNode, FileSystemError, OpenedFile, device};
use crate::{
    block::{BlockDevice, BlockError},
    memory::swap::{SwapError, SwapStorage},
};
use alloc::{boxed::Box, sync::Arc, vec::Vec};

mod file_lease;
pub(crate) use file_lease::{SwapExtent, SwapFileLayout, SwapFileLease};

/// O_RDWR source keeps the mount writer lease and the original opened entry through swapoff.
pub(crate) struct SwapSource {
    pub(in crate::fs) opened: Arc<OpenedFile>,
    pub(in crate::fs) _write_open: Option<super::vfs::WriteOpen>,
}

impl SwapSource {
    pub(crate) fn identity(&self) -> Result<crate::memory::SharedFileId, FileSystemError> {
        self.opened.inode().page_cache_id()
    }
}

pub(super) struct PreparedSwapFile {
    pub(super) layout: SwapFileLayout,
    pub(super) lease: Box<dyn SwapFileLease>,
    // Header is read through the original logical file view: unwritten page zero reads as zero.
    pub(super) header: Box<[u8; 4096]>,
}

enum StorageLease {
    Partition {
        node: Arc<BlockNode>,
        _source: SwapSource,
    },
    File {
        inode: Box<dyn SwapFileLease>,
        _source: SwapSource,
    },
}

impl StorageLease {
    fn close(&self) {
        match self {
            Self::Partition { node, .. } => node.end_swap(),
            Self::File { inode, .. } => inode.close(),
        }
    }
}

struct SwapDisk {
    identity: crate::memory::SharedFileId,
    capacity: u64,
    device: Arc<dyn BlockDevice>,
    extents: Vec<SwapExtent>,
    header: Option<Box<[u8; 4096]>>,
    // OWNER: take retires the physical or inode claim exactly once, independently of snapshot Arcs.
    lease: spin::Mutex<Option<StorageLease>>,
}

impl core::fmt::Debug for SwapDisk {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("SwapDisk")
            .field("identity", &self.identity)
            .finish()
    }
}

fn io_error(error: BlockError) -> SwapError {
    match error {
        BlockError::OutOfMemory => SwapError::OutOfMemory,
        _ => SwapError::Io,
    }
}

impl SwapDisk {
    fn physical_page(&self, page: usize) -> Result<usize, SwapError> {
        SwapExtent::physical_page(&self.extents, page).ok_or(SwapError::Invalid)
    }
}

impl SwapStorage for SwapDisk {
    fn identity(&self) -> crate::memory::SharedFileId {
        self.identity
    }
    fn size_bytes(&self) -> u64 {
        self.capacity
    }
    fn is_file(&self) -> bool {
        self.header.is_some()
    }
    fn read_header(&self, output: &mut [u8; 4096]) -> Result<(), SwapError> {
        match &self.header {
            Some(header) => {
                output.copy_from_slice(header.as_ref());
                Ok(())
            }
            None => self.read_page(0, output),
        }
    }
    fn read_page(&self, page: usize, output: &mut [u8; 4096]) -> Result<(), SwapError> {
        if self
            .device
            .read_block(self.physical_page(page)?, output)
            .map_err(io_error)?
            != 4096
        {
            return Err(SwapError::Io);
        }
        Ok(())
    }
    fn write_page(&self, page: usize, input: &[u8; 4096]) -> Result<(), SwapError> {
        if self
            .device
            .write_block(self.physical_page(page)?, input)
            .map_err(io_error)?
            != 4096
        {
            return Err(SwapError::Io);
        }
        Ok(())
    }
    fn close(&self) {
        let lease = self.lease.lock().take();
        if let Some(lease) = lease {
            lease.close();
        }
    }
}

impl Drop for SwapDisk {
    fn drop(&mut self) {
        self.close();
    }
}

/// Prepare one backing lease before header/page I/O. File swap retains the opened mount identity.
pub(crate) fn prepare_swap_storage(
    source: SwapSource,
) -> Result<Arc<dyn SwapStorage>, FileSystemError> {
    let inode = source.opened.inode();
    let identity = inode.page_cache_id()?;
    let disk = match inode.inode_type() {
        super::InodeType::BlockDevice => {
            let number = inode.device_number().ok_or(FileSystemError::NoDevice)?;
            let node = device::block_node(number).ok_or(FileSystemError::NoDevice)?;
            if node.device().block_size() != 4096 {
                return Err(FileSystemError::InvalidOperation);
            }
            let mut extents = Vec::new();
            extents
                .try_reserve_exact(1)
                .map_err(|_| FileSystemError::OutOfMemory)?;
            extents.push(SwapExtent {
                start: 0,
                end: (node.capacity() / 4096) as usize,
                physical: 0,
            });
            node.begin_swap()?;
            SwapDisk {
                identity,
                capacity: node.capacity(),
                device: node.device().clone(),
                extents,
                header: None,
                lease: spin::Mutex::new(Some(StorageLease::Partition {
                    node,
                    _source: source,
                })),
            }
        }
        super::InodeType::File => {
            let prepared = super::page_cache::prepare_swap_file(&source.opened)?;
            SwapDisk {
                identity,
                capacity: prepared.layout.size_bytes,
                device: prepared.layout.device,
                extents: prepared.layout.extents,
                header: Some(prepared.header),
                lease: spin::Mutex::new(Some(StorageLease::File {
                    inode: prepared.lease,
                    _source: source,
                })),
            }
        }
        _ => return Err(FileSystemError::InvalidOperation),
    };
    Arc::try_new(disk)
        .map(|storage| storage as Arc<dyn SwapStorage>)
        .map_err(|_| FileSystemError::OutOfMemory)
}
