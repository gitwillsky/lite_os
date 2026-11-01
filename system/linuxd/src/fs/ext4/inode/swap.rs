//! Canonical inode swap lease: freeze first, export allocated extents after durable drain.

use super::{Ext4Inode, SwapFilePhase};
use crate::fs::{
    FileSystemError, Inode, InodeType,
    swap::{SwapExtent, SwapFileLayout, SwapFileLease},
};
use alloc::{boxed::Box, sync::Arc, vec::Vec};
use spin::Mutex;

struct Ext4SwapLease {
    // OWNER: take is the single retirement point; a later Drop cannot clear a newer lease.
    inode: Mutex<Option<Arc<Ext4Inode>>>,
}

impl SwapFileLease for Ext4SwapLease {
    fn activate(&self) -> Result<SwapFileLayout, FileSystemError> {
        let inode = self
            .inode
            .lock()
            .as_ref()
            .expect("live swapfile lease")
            .clone();
        let fs = &inode.fs;
        let _mutation = fs
            .mutation
            .lock()
            .map_err(|_| FileSystemError::OutOfMemory)?;
        // All older file data must reach home blocks before any swap DMA is possible.
        fs.commit_running_transaction()?;
        let disk = inode.disk.lock().disk;
        let pages = usize::try_from(disk.size() / 4096).map_err(|_| FileSystemError::NoSpace)?;
        if pages < 2 || fs.block_size != 4096 || fs.device.block_size() != 4096 {
            return Err(FileSystemError::InvalidOperation);
        }
        let total_blocks = fs.superblock.lock().blocks_count();
        let tree = super::super::extent::ExtentTree::new(fs, inode.inode_num, &disk)?;
        let mut extents: Vec<SwapExtent> = Vec::new();
        let mut logical = 0;
        while logical < pages {
            let super::super::extent::Mapping::Mapped(extent) =
                tree.lookup(u32::try_from(logical).map_err(|_| FileSystemError::NoSpace)?)?
            else {
                return Err(FileSystemError::InvalidOperation);
            };
            let end = (extent.logical as usize + extent.length as usize).min(pages);
            let physical = usize::try_from(extent.physical)
                .ok()
                .and_then(|value| value.checked_add(logical - extent.logical as usize))
                .ok_or(FileSystemError::InvalidFileSystem)?;
            let end_physical = physical
                .checked_add(end - logical)
                .ok_or(FileSystemError::InvalidFileSystem)?;
            if end <= logical || end_physical as u64 > total_blocks {
                return Err(FileSystemError::InvalidFileSystem);
            }
            // Allocated unwritten extents are valid swap storage; do not convert them or read as holes.
            if let Some(previous) = extents.last_mut()
                && previous.physical + (previous.end - previous.start) == physical
            {
                previous.end = end;
            } else {
                extents
                    .try_reserve(1)
                    .map_err(|_| FileSystemError::OutOfMemory)?;
                extents.push(SwapExtent {
                    start: logical,
                    end,
                    physical,
                });
            }
            logical = end;
        }
        let mut state = inode.disk.lock();
        assert_eq!(state.swap, SwapFilePhase::Preparing);
        state.swap = SwapFilePhase::Active;
        drop(state);
        Ok(SwapFileLayout {
            device: fs.device.clone(),
            extents,
            size_bytes: disk.size(),
        })
    }

    fn close(&self) {
        let inode = self.inode.lock().take();
        if let Some(inode) = inode {
            inode.disk.lock().swap = SwapFilePhase::Free;
            // Last inode/filesystem references retire outside the spin guard.
            drop(inode);
        }
    }
}

impl Drop for Ext4SwapLease {
    fn drop(&mut self) {
        self.close();
    }
}

impl Ext4Inode {
    pub(super) fn freeze_swap_file(
        self: Arc<Self>,
    ) -> Result<Box<dyn SwapFileLease>, FileSystemError> {
        let mutation = self
            .fs
            .mutation
            .lock()
            .map_err(|_| FileSystemError::OutOfMemory)?;
        if self.inode_type() != InodeType::File {
            return Err(FileSystemError::InvalidOperation);
        }
        let mut state = self.disk.lock();
        if state.swap != SwapFilePhase::Free {
            return Err(FileSystemError::Busy);
        }
        state.swap = SwapFilePhase::Preparing;
        drop(state);
        drop(mutation);
        // A failed allocation drops this already-owning lease and clears only its own freeze.
        Box::try_new(Ext4SwapLease {
            inode: Mutex::new(Some(self)),
        })
        .map(|lease| lease as Box<dyn SwapFileLease>)
        .map_err(|_| FileSystemError::OutOfMemory)
    }

    /// Called inside the filesystem mutation domain before changing data or layout.
    pub(in crate::fs::ext4) fn check_swap_mutation(
        &self,
        writeback: bool,
    ) -> Result<(), FileSystemError> {
        match self.disk.lock().swap {
            SwapFilePhase::Free => Ok(()),
            SwapFilePhase::Preparing if writeback => Ok(()),
            SwapFilePhase::Preparing | SwapFilePhase::Active => Err(FileSystemError::TextBusy),
        }
    }
}
