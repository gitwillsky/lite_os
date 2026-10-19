//! Freeze/durable drain follows write_sequence -> operation -> filesystem mutation; never operation -> mm.
use super::*;
use crate::fs::{OpenedFile, swap::PreparedSwapFile};
use alloc::boxed::Box;

pub(in crate::fs) fn prepare_swap_file(
    opened: &Arc<OpenedFile>,
) -> Result<PreparedSwapFile, FileSystemError> {
    let file = cached_file(opened.inode())?.ok_or(FileSystemError::InvalidOperation)?;
    let mut wait = TaskMutexWaitPreparation::prepare().map_err(|_| FileSystemError::OutOfMemory)?;
    let mut header = Box::try_new([0u8; 4096]).map_err(|_| FileSystemError::OutOfMemory)?;
    let _sequence = file
        .write_sequence
        .lock()
        .map_err(|_| FileSystemError::OutOfMemory)?;
    let operation = file
        .operation
        .lock()
        .map_err(|_| FileSystemError::OutOfMemory)?;
    let lease = crate::fs::vfs().pin_swap_file(opened)?;
    drop(operation);
    // 1. Fault uses mm -> operation. Drop operation before all-CPU writer retirement callbacks.
    crate::memory::revoke_shared_file_writers(file.id, &mut wait);
    // 2. An exiting mm can have retired its Weak identity before its writer fields are destroyed.
    // Wait on the same actual page state; no shadow aggregate counter or allocation on the tail.
    loop {
        let writers = file
            .pages
            .lock()
            .entries
            .values()
            .any(|page| page.state.load(Ordering::Acquire) & PAGE_WRITER_MASK != 0);
        if !writers {
            break;
        }
        file.writer_retired.wait();
    }
    // 3. No new writer may publish after freeze; cache batch writeback alone can drain Preparing.
    let _operation = file.operation.lock_prepared(&mut wait);
    file.writeback_range(0, u64::MAX)?;
    if file.pages.lock().entries.values().any(|page| page.dirty()) {
        return Err(FileSystemError::IoError);
    }
    if file.inode.read_storage(0, header.as_mut())? != 4096 {
        return Err(FileSystemError::InvalidOperation);
    }
    let layout = lease.activate()?;
    Ok(PreparedSwapFile {
        layout,
        lease,
        header,
    })
}
