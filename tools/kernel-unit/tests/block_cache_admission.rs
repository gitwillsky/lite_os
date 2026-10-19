#![feature(allocator_ext)]
//! 执行生产 claim、cache registry 和 RegularFile facade；只替换 frame/storage/scheduler adapter。
#![allow(dead_code)]

extern crate alloc;
use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::{Cell, RefCell},
};

thread_local! {
    static ALLOCATION_HOOK: Cell<Option<fn()>> = const { Cell::new(None) };
    static FAIL_ALLOCATION: Cell<bool> = const { Cell::new(false) };
    static REGISTRATION_HOOK: RefCell<Option<Box<dyn FnOnce()>>> = const { RefCell::new(None) };
}
struct FaultAllocator;
// SAFETY: 成功内存原样委托 System；一次线程局部故障/回调只影响当前测试的真实 allocation。
unsafe impl GlobalAlloc for FaultAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if let Some(hook) = ALLOCATION_HOOK.try_with(|hook| hook.take()).ok().flatten() {
            hook();
        }
        if FAIL_ALLOCATION
            .try_with(|flag| flag.replace(false))
            .unwrap_or(false)
        {
            return core::ptr::null_mut();
        }
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) }
    }
}
#[global_allocator]
static ALLOCATOR: FaultAllocator = FaultAllocator;

#[path = "../../../kernel/src/fallible_tree/mod.rs"]
mod fallible_tree;

#[path = "../../../kernel/src/fs"]
mod fs {
    use crate::fallible_tree::FallibleMap;
    use alloc::sync::Arc;
    use spin::Mutex;
    const PAGE_SIZE: usize = 4096;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum FileSystemError {
        OutOfMemory,
        InvalidOperation,
        AlreadyExists,
        Busy,
        ReadOnly,
        TextBusy,
    }
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum InodeType {
        File,
        BlockDevice,
        Directory,
    }
    #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
    struct SharedFileId {
        filesystem: usize,
        inode: u64,
    }
    struct Metadata {
        inode: u64,
        links: usize,
    }
    enum DataBacking {
        PageCache,
        Snapshot,
        Memory(Arc<MemoryFile>),
    }
    trait Inode: Send + Sync {
        fn inode_type(&self) -> InodeType;
        fn filesystem_id(&self) -> usize;
        fn page_cache_id(&self) -> Result<SharedFileId, FileSystemError>;
        fn data_backing(&self) -> DataBacking;
        fn size(&self) -> u64;
        fn is_swap_file(&self) -> bool {
            false
        }
        fn metadata(&self) -> Result<Metadata, FileSystemError>;
        fn notification_link_state(&self) -> Option<(u64, u32)> {
            self.metadata()
                .ok()
                .map(|value| (value.inode, value.links as u32))
        }
        fn read_storage(&self, offset: u64, output: &mut [u8]) -> Result<usize, FileSystemError>;
    }
    mod block_claim;
    use block_claim::BlockClaim;

    struct AliasInode {
        claim: BlockClaim,
        bytes: Arc<Mutex<[u8; PAGE_SIZE]>>,
        id: SharedFileId,
    }
    impl Inode for AliasInode {
        fn inode_type(&self) -> InodeType {
            InodeType::BlockDevice
        }
        fn filesystem_id(&self) -> usize {
            self.id.filesystem
        }
        fn page_cache_id(&self) -> Result<SharedFileId, FileSystemError> {
            Ok(self.id)
        }
        // 设备适配：策略本身由原始 BlockClaim owner 计算，与 BlockSpecial 的投影一致。
        fn data_backing(&self) -> DataBacking {
            if self.claim.bypass_cache() {
                DataBacking::Snapshot
            } else {
                DataBacking::PageCache
            }
        }
        fn size(&self) -> u64 {
            PAGE_SIZE as u64
        }
        fn metadata(&self) -> Result<Metadata, FileSystemError> {
            Ok(Metadata {
                inode: self.id.inode,
                links: 1,
            })
        }
        fn read_storage(&self, offset: u64, output: &mut [u8]) -> Result<usize, FileSystemError> {
            let bytes = self.bytes.lock();
            let start = offset as usize;
            let count = output.len().min(PAGE_SIZE.saturating_sub(start));
            output[..count].copy_from_slice(&bytes[start..start + count]);
            Ok(count)
        }
    }

    struct OrphanInode(SharedFileId);
    impl Inode for OrphanInode {
        fn inode_type(&self) -> InodeType {
            InodeType::File
        }
        fn filesystem_id(&self) -> usize {
            self.0.filesystem
        }
        fn page_cache_id(&self) -> Result<SharedFileId, FileSystemError> {
            Ok(self.0)
        }
        fn data_backing(&self) -> DataBacking {
            DataBacking::PageCache
        }
        fn size(&self) -> u64 {
            0
        }
        fn metadata(&self) -> Result<Metadata, FileSystemError> {
            Ok(Metadata {
                inode: self.0.inode,
                links: 0,
            })
        }
        fn read_storage(&self, _: u64, _: &mut [u8]) -> Result<usize, FileSystemError> {
            Ok(0)
        }
    }

    // 冷生产 owner 的 page/scheduler adapter；不替代 registry 或 facade 的 admission/read 算法。
    struct TaskMutex<T>(Mutex<T>);
    type TaskMutexGuard<'a, T> = spin::MutexGuard<'a, T>;
    impl<T> TaskMutex<T> {
        fn new(value: T) -> Self {
            Self(Mutex::new(value))
        }
        fn lock(&self) -> Result<TaskMutexGuard<'_, T>, ()> {
            Ok(self.0.lock())
        }
    }
    struct Frame([u8; PAGE_SIZE]);
    impl Frame {
        fn read(&self, offset: usize, output: &mut [u8]) {
            output.copy_from_slice(&self.0[offset..offset + output.len()]);
        }
    }
    struct CachedPage {
        frame: Frame,
    }
    impl CachedPage {
        fn dirty(&self) -> bool {
            false
        }
        fn reclaimable(&self) -> bool {
            true
        }
    }
    struct CachedPages {
        entries: FallibleMap<u64, Arc<CachedPage>>,
    }
    impl CachedPages {
        fn new() -> Self {
            Self {
                entries: FallibleMap::new(),
            }
        }
    }
    struct TaskEvent;
    impl TaskEvent {
        fn new() -> Self {
            Self
        }
    }
    struct CachedFile {
        id: SharedFileId,
        inode: Arc<dyn Inode>,
        operation: TaskMutex<()>,
        write_sequence: TaskMutex<()>,
        pages: Mutex<CachedPages>,
        writer_retired: Arc<TaskEvent>,
    }
    impl CachedFile {
        fn page_with_storage(
            &self,
            index: u64,
        ) -> Result<(Arc<CachedPage>, usize), FileSystemError> {
            let mut pages = self.pages.lock();
            if let Some(page) = pages.entries.get(&index) {
                return Ok((page.clone(), 0));
            }
            let mut frame = Frame([0; PAGE_SIZE]);
            let filled = self
                .inode
                .read_storage(index * PAGE_SIZE as u64, &mut frame.0)?;
            let page = Arc::new(CachedPage { frame });
            pages
                .entries
                .try_insert(index, page.clone())
                .map_err(|_| FileSystemError::OutOfMemory)?;
            Ok((page, filled))
        }
        fn writeback_range(&self, _: u64, _: u64) -> Result<(), FileSystemError> {
            Ok(())
        }
    }
    struct MemoryFile;
    struct MemoryWrite<'a>(&'a MemoryFile);
    impl MemoryFile {
        fn id(&self) -> SharedFileId {
            unreachable!()
        }
        fn size(&self) -> u64 {
            unreachable!()
        }
        fn read(&self, _: u64, _: &mut [u8]) -> Result<usize, FileSystemError> {
            unreachable!()
        }
        fn begin_write(&self) -> Result<MemoryWrite<'_>, FileSystemError> {
            unreachable!()
        }
    }
    // Permission values are a seam: this fixture only admits block devices, never regular modes.
    struct AccessIdentity;
    struct MemoryInodeWrite<'a> {
        write: MemoryWrite<'a>,
        inode: &'a Arc<dyn Inode>,
        identity: AccessIdentity,
    }
    enum RegularFileWrite<'a> {
        Cached(CachedWrite<'a>),
        Memory(MemoryInodeWrite<'a>),
    }
    struct CachedWrite<'a> {
        file: &'a CachedFile,
        identity: AccessIdentity,
        _sequence: TaskMutexGuard<'a, ()>,
    }
    struct PageCacheStatistics {
        resident_pages: usize,
        dirty_pages: usize,
        reclaimable_pages: usize,
    }
    fn shared_error(_: ()) -> FileSystemError {
        FileSystemError::OutOfMemory
    }
    // Registration 本身另有 memory_owner_registry 的生产测试；本 fixture 只注入准备中的调度点。
    fn register_memory_reclaimer(_: Arc<CachedFile>) -> Result<(), ()> {
        let hook = crate::REGISTRATION_HOOK.with(|hook| hook.borrow_mut().take());
        if let Some(hook) = hook {
            hook();
        }
        Ok(())
    }

    #[path = "page_cache/registry.rs"]
    mod registry;
    use registry::cached_file;
    #[path = "page_cache/regular_file.rs"]
    mod regular_file;
    use regular_file::RegularFile;
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    fn setup(key: u64) -> (BlockClaim, Arc<AliasInode>, Arc<Mutex<[u8; PAGE_SIZE]>>) {
        let whole = BlockClaim::whole(key, PAGE_SIZE as u64).unwrap();
        let alias = whole.partition(key + 1, 0, PAGE_SIZE as u64).unwrap();
        let bytes = Arc::new(Mutex::new([0x31; PAGE_SIZE]));
        let inode = Arc::new(AliasInode {
            claim: alias,
            bytes: bytes.clone(),
            id: SharedFileId {
                filesystem: 900,
                inode: key + 1,
            },
        });
        (whole, inode, bytes)
    }

    #[test]
    fn final_dput_detaches_registry_while_actual_io_pin_keeps_inode_alive() {
        let _serial = SERIAL.lock().unwrap();
        let inode: Arc<dyn Inode> = Arc::new(OrphanInode(SharedFileId {
            filesystem: 901,
            inode: 1,
        }));
        let retired = Arc::downgrade(&inode);
        let worker = cached_file(inode.clone()).unwrap().unwrap();
        registry::evict_if_unlinked(&inode);
        // 直接经生产 admission 查 registry，临时 I/O 仍存活也必须已摘除旧 entry。
        let fresh = cached_file(inode.clone()).unwrap().unwrap();
        assert!(!Arc::ptr_eq(&worker, &fresh));
        drop(worker);
        registry::evict_if_unlinked(&inode);
        drop(inode);
        assert!(
            retired.upgrade().is_some(),
            "I/O Arc must keep the inode stable"
        );
        drop(fresh);
        assert!(
            retired.upgrade().is_none(),
            "last I/O must reclaim without another dput"
        );
    }

    #[test]
    fn alias_reader_cannot_recreate_cache_after_foreign_writer_eviction() {
        let _serial = SERIAL.lock().unwrap();
        let (whole, inode, bytes) = setup(10);
        let mut output = [0; 32];
        let warm = RegularFile::from_inode(inode.clone()).unwrap();
        assert!(warm.id().is_some());
        warm.read(0, &mut output).unwrap();
        assert_eq!(output, [0x31; 32]);
        drop(warm);
        whole.begin_writer().unwrap();
        registry::evict_cached(inode.id).unwrap();
        let reader = RegularFile::from_inode(inode.clone()).unwrap();
        assert!(
            reader.id().is_none(),
            "foreign writer must prevent a new alias cache"
        );
        *bytes.lock() = [0x72; PAGE_SIZE];
        reader.read(0, &mut output).unwrap();
        assert_eq!(output, [0x72; 32]);
        drop(reader);
        whole.end_writer();
        let next = RegularFile::from_inode(inode.clone()).unwrap();
        next.read(0, &mut output).unwrap();
        assert_eq!(output, [0x72; 32]);
        drop(next);
        registry::evict_cached(inode.id).unwrap();
    }

    #[test]
    fn lease_acquired_during_preparation_prevents_delayed_cache_publication() {
        let _serial = SERIAL.lock().unwrap();
        let (whole, inode, bytes) = setup(30);
        let before = Arc::new(std::sync::Barrier::new(2));
        let resume = Arc::new(std::sync::Barrier::new(2));
        let reader_inode = inode.clone();
        let (reader_before, reader_resume) = (before.clone(), resume.clone());
        let reader = std::thread::spawn(move || {
            crate::REGISTRATION_HOOK.with(|hook| {
                *hook.borrow_mut() = Some(Box::new(move || {
                    reader_before.wait();
                    reader_resume.wait();
                }))
            });
            RegularFile::from_inode(reader_inode)
        });
        before.wait();
        whole.begin_writer().unwrap();
        registry::evict_cached(inode.id).unwrap();
        *bytes.lock() = [0x65; PAGE_SIZE];
        resume.wait();
        let reader = reader.join().unwrap().unwrap();
        assert!(
            reader.id().is_none(),
            "prepared cache bypassed the new physical lease"
        );
        let mut output = [0; 32];
        reader.read(0, &mut output).unwrap();
        assert_eq!(output, [0x65; 32]);
        drop(reader);
        whole.end_writer();
        registry::evict_cached(inode.id).unwrap();
    }

    fn reenter_registry() {
        let _ = registry::statistics();
    }
    fn fail_and_reenter_registry() {
        reenter_registry();
        crate::FAIL_ALLOCATION.with(|flag| flag.set(true));
    }
    fn fail_second_allocation() {
        crate::ALLOCATION_HOOK.with(|hook| hook.set(Some(fail_and_reenter_registry)));
    }
    #[test]
    fn cache_preparation_oom_can_reenter_registry_and_retry_without_publication() {
        if std::env::var_os("LITEOS_CACHE_REENTRY_CHILD").is_none() {
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "fs::cache_preparation_oom_can_reenter_registry_and_retry_without_publication",
                ])
                .env("LITEOS_CACHE_REENTRY_CHILD", "1")
                .spawn()
                .unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                if let Some(status) = child.try_wait().unwrap() {
                    assert!(status.success());
                    return;
                }
                if std::time::Instant::now() >= deadline {
                    child.kill().unwrap();
                    child.wait().unwrap();
                    panic!("cache allocation reentered a locked registry");
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
        let (_whole, inode, _) = setup(50);
        crate::ALLOCATION_HOOK.with(|hook| hook.set(Some(reenter_registry)));
        crate::FAIL_ALLOCATION.with(|flag| flag.set(true));
        assert!(matches!(
            RegularFile::from_inode(inode.clone()),
            Err(FileSystemError::OutOfMemory)
        ));
        assert_eq!(registry::statistics().resident_pages, 0);
        let file = RegularFile::from_inode(inode.clone()).unwrap();
        assert!(file.id().is_some());
        drop(file);
        registry::evict_cached(inode.id).unwrap();
    }

    #[test]
    fn cache_arc_oom_drops_prepared_node_outside_registry_before_retry() {
        let _serial = SERIAL.lock().unwrap();
        let (_whole, inode, _) = setup(70);
        crate::ALLOCATION_HOOK.with(|hook| hook.set(Some(fail_second_allocation)));
        assert!(matches!(
            RegularFile::from_inode(inode.clone()),
            Err(FileSystemError::OutOfMemory)
        ));
        let file = RegularFile::from_inode(inode.clone()).unwrap();
        assert!(file.id().is_some());
        drop(file);
        registry::evict_cached(inode.id).unwrap();
    }

    #[test]
    fn concurrent_cache_candidates_publish_one_winner() {
        let _serial = SERIAL.lock().unwrap();
        let (_whole, inode, _) = setup(90);
        let ready = Arc::new(std::sync::Barrier::new(2));
        let workers: Vec<_> = (0..2)
            .map(|_| {
                let inode = inode.clone();
                let ready = ready.clone();
                std::thread::spawn(move || {
                    crate::REGISTRATION_HOOK.with(|hook| {
                        *hook.borrow_mut() = Some(Box::new(move || {
                            ready.wait();
                        }));
                    });
                    cached_file(inode).unwrap().unwrap()
                })
            })
            .collect();
        let winners: Vec<_> = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect();
        assert!(Arc::ptr_eq(&winners[0], &winners[1]));
        drop(winners);
        registry::evict_cached(inode.id).unwrap();
    }
}
