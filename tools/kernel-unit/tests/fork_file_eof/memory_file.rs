// Execute MemoryFile, SparsePages, StorageBudget, canonical Page and actual swap storage owner.

#[test]
fn unmapped_memory_file_swap_preserves_quota_short_io_and_atomic_tail_truncate() {
    use crate::fs::{FileSystemError, QuotaFile};
    use crate::memory::swap;
    use core::sync::atomic::Ordering;
    let _serial = SWAP_TEST.lock().unwrap();
    let request = background_request();
    let disk = SwapDisk::new(9);
    swap::activate(disk.clone(), b"/dev/test", 0).unwrap();
    let owner = QuotaFile::new(2);
    let file = &owner.file;
    let mut bytes = vec![0x31; config::PAGE_SIZE];
    bytes.extend(vec![0x72; config::PAGE_SIZE]);
    assert_eq!(
        file.begin_write().unwrap().write(0, &bytes).unwrap(),
        bytes.len()
    );
    assert_eq!(
        file.reclaim_pages(ReclaimRequest::for_target(1))
            .reclaimed_pages(),
        0
    );
    for _ in 0..2 {
        assert_eq!(file.reclaim_pages(request).reclaimed_pages(), 1);
    }
    assert_eq!(swap::statistics(), (7, 5));
    assert_eq!(
        (owner.used_pages(), file.allocated_pages().unwrap()),
        (2, 2)
    );
    assert_eq!(
        file.begin_write().unwrap().write(8192, b"over quota"),
        Err(FileSystemError::NoSpace)
    );
    assert_eq!(file.size(), 8192);

    // Only the first content page is restored; a later failed page-in returns its completed prefix.
    let mut first = vec![0; config::PAGE_SIZE];
    assert_eq!(file.read(0, &mut first).unwrap(), config::PAGE_SIZE);
    assert_eq!(first, vec![0x31; config::PAGE_SIZE]);
    assert_eq!(swap::statistics(), (7, 6));
    disk.read_error.store(true, Ordering::Relaxed);
    let mut output = vec![0xdd; 8192];
    assert_eq!(file.read(0, &mut output).unwrap(), config::PAGE_SIZE);
    assert_eq!(&output[..4096], first.as_slice());
    assert_eq!(&output[4096..], &[0xdd; 4096]);
    assert_eq!(
        file.begin_write().unwrap().write(4096, b"fail"),
        Err(FileSystemError::IoError)
    );
    assert_eq!(owner.truncate(4096 + 13), Err(FileSystemError::IoError));
    assert_eq!(
        (
            file.size(),
            file.allocated_pages().unwrap(),
            owner.used_pages()
        ),
        (8192, 2, 2)
    );
    assert_eq!(swap::statistics(), (7, 6));

    disk.read_error.store(false, Ordering::Relaxed);
    owner.truncate(4096 + 13).unwrap();
    owner.truncate(8192).unwrap();
    assert_eq!(file.read(0, &mut output).unwrap(), 8192);
    assert_eq!(&output[..4096], first.as_slice());
    assert_eq!(&output[4096..4109], &[0x72; 13]);
    assert!(output[4109..].iter().all(|&byte| byte == 0));
    assert_eq!(swap::statistics(), (7, 7));

    // Storage reclaim still covers content with no mmap record; swapoff restores those owners.
    assert_eq!(file.reclaim_pages(request).reclaimed_pages(), 1);
    let mut wait = crate::sync::TaskMutexWaitPreparation;
    swap::deactivate(
        SharedFileId {
            filesystem: 999,
            inode: 1,
        },
        &mut wait,
    )
    .unwrap();
    assert!(disk.closed.load(Ordering::Acquire));
    let mut restored = vec![0; 8192];
    assert_eq!(file.read(0, &mut restored).unwrap(), 8192);
    assert_eq!(restored, output);
    owner.truncate(0).unwrap();
    assert_eq!(
        (file.allocated_pages().unwrap(), owner.used_pages()),
        (0, 0)
    );
}

#[test]
fn actual_memory_mtime_is_one_owner_and_explicit_past_time_survives_projection() {
    let _serial = SWAP_TEST.lock().unwrap();
    let owner = crate::fs::QuotaFile::new(1);
    assert_eq!(owner.times().1, 1);
    owner.set_modified(0);
    assert_eq!(owner.times().1, 0);
    owner.file.begin_write().unwrap().write(0, b"updated").unwrap();
    assert_eq!(owner.times().1, 1);
}

#[test]
fn actual_memory_future_mtime_does_not_become_ctime() {
    let _serial = SWAP_TEST.lock().unwrap();
    let owner=crate::fs::QuotaFile::new(1);
    owner.set_modified(4_000_000_000);
    assert_eq!(owner.times(),(1,4_000_000_000,1));
    owner.file.begin_write().unwrap().write(0,b"touch").unwrap();
    assert_eq!(owner.times(),(1,1,1));
}

#[test]
fn sealed_resize_rejects_before_metadata_preparation_and_commit() {
    let _serial = SWAP_TEST.lock().unwrap();
    use std::cell::Cell;
    use crate::fs::{QuotaFile,FileSystemError};
    let owner=QuotaFile::new_with_sealing(1,true);
    owner.file.begin_write().unwrap().write(0,b"kept").unwrap();
    owner.file.add_seals(2|4).unwrap();
    let before=owner.times();
    let prepared=Cell::new(0); let committed=Cell::new(0);
    for size in [0,5] {
        assert_eq!(owner.truncate_transaction(size,||{prepared.set(prepared.get()+1);Ok(())},|_|committed.set(committed.get()+1)),Err(FileSystemError::PermissionDenied));
        assert_eq!(owner.file.size(),4); assert_eq!(owner.times(),before);
    }
    assert_eq!((prepared.get(),committed.get()),(0,0));
    owner.truncate_transaction(4,||{prepared.set(1);Ok(())},|_|committed.set(1)).unwrap();
    assert_eq!((prepared.get(),committed.get()),(1,1));
}
#[test]
fn metadata_preparation_failure_does_not_resize_or_touch_times() {
    let _serial = SWAP_TEST.lock().unwrap();
    use crate::fs::{QuotaFile,FileSystemError};
    let owner=QuotaFile::new(1);
    owner.file.begin_write().unwrap().write(0,b"kept").unwrap();
    owner.set_modified(8);
    let before=owner.times();
    assert_eq!(owner.truncate_transaction::<()>(0,||Err(FileSystemError::OutOfMemory),|_|panic!("failure must not commit metadata")),Err(FileSystemError::OutOfMemory));
    assert_eq!(owner.file.size(),4); assert_eq!(owner.times(),before);
}
