#![feature(allocator_ext)]
#![allow(dead_code)]
//! Production OFD prepare/initialize owner. OFD payload/close callback, mount and device are explicit seams.
extern crate alloc;
#[path = "../src/allocation_failure.rs"]
mod allocation_failure;
mod fs {
    use alloc::sync::Arc;
    use core::sync::atomic::AtomicUsize;
    #[derive(Debug, PartialEq, Eq)]
    pub(crate) enum FileSystemError {
        OutOfMemory,
        ReadOnly,
        NoDevice,
    }
    #[derive(PartialEq, Eq)]
    pub(crate) enum InodeType {
        File,
        BlockDevice,
    }
    pub(crate) const O_ACCMODE: u32 = 3;
    pub(crate) const O_RDONLY: u32 = 0;
    pub(crate) const O_PATH: u32 = 0x200000;
    pub(crate) struct OpenedFile {
        pub(crate) closes: AtomicUsize,
    }
    pub(crate) struct Inode;
    impl OpenedFile {
        pub(crate) fn begin_content_write(&self) -> Result<ContentWrite, FileSystemError> {
            vfs().begin_write_open(42)
        }
        pub(crate) fn inode(&self) -> Inode {
            Inode
        }
    }
    impl Inode {
        pub(crate) fn filesystem_id(&self) -> usize {
            42
        }
        pub(crate) fn inode_type(&self) -> InodeType {
            InodeType::File
        }
        pub(crate) fn device_number(&self) -> Option<u64> {
            None
        }
    }
    pub(crate) fn anonymous_opened(_: &[u8]) -> Result<Arc<OpenedFile>, FileSystemError> {
        unreachable!(
            "this fixture covers storage preparation; anonymous inode publication is covered by the actual anonymous owner fixture"
        )
    }
    pub(crate) enum InodeOpen {
        Storage,
        Device(Arc<dyn device::DeviceFile>),
    }
    std::thread_local! {
        static WRITABLE: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
        static WRITERS: core::cell::Cell<usize> = const { core::cell::Cell::new(0) };
    }
    pub(crate) fn allow_writes(allowed: bool) {
        WRITABLE.with(|value| value.set(allowed));
    }
    pub(crate) fn writers() -> usize {
        WRITERS.with(|value| value.get())
    }
    pub(crate) struct ContentWrite;
    impl Drop for ContentWrite {
        fn drop(&mut self) {
            WRITERS.with(|value| value.set(value.get() - 1));
        }
    }
    pub(crate) struct BlockNode;
    impl BlockNode {
        pub(crate) fn begin_writer(&self) -> Result<(), FileSystemError> {
            Ok(())
        }
    }
    pub(crate) struct Vfs;
    impl Vfs {
        pub(crate) fn begin_write_open(&self, _: usize) -> Result<ContentWrite, FileSystemError> {
            if !WRITABLE.with(|value| value.get()) {
                return Err(FileSystemError::ReadOnly);
            }
            WRITERS.with(|value| value.set(value.get() + 1));
            Ok(ContentWrite)
        }
    }
    pub(crate) fn vfs() -> &'static Vfs {
        &Vfs
    }
    pub(crate) mod device {
        pub(crate) trait DeviceFile {}
        pub(crate) fn block_node(_: u64) -> Option<alloc::sync::Arc<super::BlockNode>> {
            None
        }
    }
}
#[path = "../../../kernel/src/file"]
mod file {
    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicUsize, Ordering};
    use spin::Mutex;
    pub(crate) struct FilePosition;
    impl FilePosition {
        pub(crate) fn new() -> Self {
            Self
        }
    }
    pub(crate) struct EpollMemberships;
    impl EpollMemberships {
        pub(crate) fn new() -> Self {
            Self
        }
    }
    pub(crate) enum OpenFileKind {
        Inode,
        Device(Arc<dyn crate::fs::device::DeviceFile>),
    }
    pub(crate) struct WritableInodeLease {
        pub(crate) _content: crate::fs::ContentWrite,
        pub(crate) block: Option<Arc<crate::fs::BlockNode>>,
    }
    pub(crate) struct OpenFileDescription {
        pub(crate) kind: OpenFileKind,
        pub(crate) position: FilePosition,
        pub(crate) flags: Mutex<u32>,
        pub(crate) opened: Arc<crate::fs::OpenedFile>,
        pub(crate) epoll_memberships: EpollMemberships,
        pub(crate) descriptor_refs: AtomicUsize,
        pub(crate) _write_open: Option<WritableInodeLease>,
    }
    impl Drop for OpenFileDescription {
        fn drop(&mut self) {
            self.opened.closes.fetch_add(1, Ordering::Relaxed);
        }
    }
    mod description_preparation;
    pub(crate) use description_preparation::PreparedFileDescription;
}
fn opened() -> alloc::sync::Arc<fs::OpenedFile> {
    alloc::sync::Arc::new(fs::OpenedFile {
        closes: core::sync::atomic::AtomicUsize::new(0),
    })
}
#[test]
fn actual_ofd_prepare_oom_does_not_construct_or_close_payload() {
    use core::sync::atomic::Ordering;
    let opened = opened();
    allocation_failure::fail_next();
    assert!(matches!(
        file::PreparedFileDescription::try_new(),
        Err(fs::FileSystemError::OutOfMemory)
    ));
    assert!(allocation_failure::was_consumed());
    assert_eq!(opened.closes.load(Ordering::Relaxed), 0);
    assert_eq!(alloc::sync::Arc::strong_count(&opened), 1);
}
#[test]
fn actual_uninitialized_abort_has_no_close_and_initialized_owner_closes_exactly_once() {
    use core::sync::atomic::Ordering;
    let opened = opened();
    drop(file::PreparedFileDescription::try_new().unwrap());
    assert_eq!(opened.closes.load(Ordering::Relaxed), 0);
    let prepared = file::PreparedFileDescription::try_new().unwrap();
    allocation_failure::fail_next();
    let ofd = prepared
        .prepare_named(opened.clone(), fs::O_RDONLY)
        .unwrap()
        .finish(fs::InodeOpen::Storage);
    assert!(
        !allocation_failure::was_consumed(),
        "initialize must not allocate"
    );
    let pin = ofd.clone();
    drop(ofd);
    assert_eq!(opened.closes.load(Ordering::Relaxed), 0);
    drop(pin);
    assert_eq!(opened.closes.load(Ordering::Relaxed), 1);
}
#[test]
fn actual_prepared_lease_failure_cannot_close_a_file_that_never_opened() {
    use core::sync::atomic::Ordering;
    let opened = opened();
    let prepared = file::PreparedFileDescription::try_new().unwrap();
    assert!(matches!(
        prepared.prepare_named(opened.clone(), 1),
        Err(fs::FileSystemError::ReadOnly)
    ));
    assert_eq!(opened.closes.load(Ordering::Relaxed), 0);
    assert_eq!(alloc::sync::Arc::strong_count(&opened), 1);
}

#[test]
fn actual_named_abort_returns_writer_without_constructing_close_lifecycle() {
    use core::sync::atomic::Ordering;
    fs::allow_writes(true);
    let opened = opened();
    let prepared = file::PreparedFileDescription::try_new()
        .unwrap()
        .prepare_named(opened.clone(), 1)
        .unwrap();
    assert_eq!(fs::writers(), 1);
    drop(prepared); // Backend failure: no successful open to close.
    assert_eq!(fs::writers(), 0);
    assert_eq!(opened.closes.load(Ordering::Relaxed), 0);
    fs::allow_writes(false);
}
#[test]
fn actual_regular_device_operations_hold_the_same_mount_writer_as_storage() {
    use core::sync::atomic::Ordering;
    struct ProcOperations;
    impl fs::device::DeviceFile for ProcOperations {}
    fs::allow_writes(true);
    let opened = opened();
    let prepared = file::PreparedFileDescription::try_new()
        .unwrap()
        .prepare_named(opened.clone(), 1)
        .unwrap();
    assert_eq!(fs::writers(), 1);
    let ofd = prepared.finish(fs::InodeOpen::Device(alloc::sync::Arc::new(ProcOperations)));
    assert_eq!(fs::writers(), 1);
    drop(ofd);
    assert_eq!(fs::writers(), 0);
    assert_eq!(opened.closes.load(Ordering::Relaxed), 1);
    fs::allow_writes(false);
}

#[test]
fn actual_path_preparation_never_acquires_a_writer_or_device_backend() {
    fs::allow_writes(false);
    let opened = opened();
    let prepared = file::PreparedFileDescription::try_new().unwrap();
    allocation_failure::fail_next();
    let path = prepared.finish_path(opened.clone(), fs::O_PATH);
    assert!(
        !allocation_failure::was_consumed(),
        "path commit must not allocate"
    );
    assert!(matches!(path.kind, file::OpenFileKind::Inode));
    assert!(path._write_open.is_none());
    assert_eq!(*path.flags.lock(), fs::O_PATH);
    assert_eq!(fs::writers(), 0);
}
