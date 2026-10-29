static SWAP_TEST: std::sync::Mutex<()> = std::sync::Mutex::new(());
struct SwapDisk {
    identifier: u64,
    pages: std::sync::Mutex<Vec<[u8; 4096]>>,
    write_error: core::sync::atomic::AtomicBool,
    read_error: core::sync::atomic::AtomicBool,
    closed: core::sync::atomic::AtomicBool,
    write_hook: std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>>,
    read_hook: std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>>,
}
impl core::fmt::Debug for SwapDisk {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SwapDisk")
            .field("identifier", &self.identifier)
            .finish_non_exhaustive()
    }
}
impl SwapDisk {
    fn new(total: usize) -> Arc<Self> {
        Self::with_identity(total, 1)
    }
    fn with_identity(total: usize, identifier: u64) -> Arc<Self> {
        let mut pages = vec![[0u8; 4096]; total];
        pages[0][4086..].copy_from_slice(b"SWAPSPACE2");
        pages[0][1024..1028].copy_from_slice(&1u32.to_ne_bytes());
        pages[0][1028..1032].copy_from_slice(&((total - 1) as u32).to_ne_bytes());
        pages[0][1032..1036].copy_from_slice(&1u32.to_ne_bytes());
        pages[0][1536..1540].copy_from_slice(&2u32.to_ne_bytes());
        Arc::new(Self {
            identifier,
            pages: std::sync::Mutex::new(pages),
            write_error: false.into(),
            read_error: false.into(),
            closed: false.into(),
            write_hook: std::sync::Mutex::new(None),
            read_hook: std::sync::Mutex::new(None),
        })
    }
}
impl crate::memory::swap::SwapStorage for SwapDisk {
    fn identity(&self) -> SharedFileId {
        SharedFileId {
            filesystem: 999,
            inode: self.identifier,
        }
    }
    fn size_bytes(&self) -> u64 {
        self.pages.lock().unwrap().len() as u64 * 4096
    }
    fn read_page(
        &self,
        page: usize,
        output: &mut [u8; 4096],
    ) -> Result<(), crate::memory::swap::SwapError> {
        assert!(!self.closed.load(core::sync::atomic::Ordering::Acquire));
        if self.read_error.load(core::sync::atomic::Ordering::Relaxed) {
            return Err(crate::memory::swap::SwapError::Io);
        }
        output.copy_from_slice(&self.pages.lock().unwrap()[page]);
        if let Some(hook) = self.read_hook.lock().unwrap().take() {
            hook();
        }
        Ok(())
    }
    fn write_page(
        &self,
        page: usize,
        input: &[u8; 4096],
    ) -> Result<(), crate::memory::swap::SwapError> {
        let hook = self.write_hook.lock().unwrap().take();
        if let Some(hook) = hook {
            hook();
        }
        assert!(
            page != 0 && page != 2,
            "header and bad page must never become slots"
        );
        assert!(!self.closed.load(core::sync::atomic::Ordering::Acquire));
        if self.write_error.load(core::sync::atomic::Ordering::Relaxed) {
            return Err(crate::memory::swap::SwapError::OutOfMemory);
        }
        self.pages.lock().unwrap()[page].copy_from_slice(input);
        Ok(())
    }
    fn close(&self) {
        self.closed
            .store(true, core::sync::atomic::Ordering::Release);
    }
}
type RetirementHook = Box<dyn FnOnce(&MemorySet) + Send>;
struct SwapMemory {
    memory: crate::execution::sync::TaskMutex<MemorySet>,
    before_restore: std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>>,
    before_revoke: std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>>,
    after_revoke: std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>>,
    before_drop: std::sync::Mutex<Option<RetirementHook>>,
}
impl SwapMemory {
    fn new(memory: MemorySet) -> Self {
        Self {
            memory: crate::execution::sync::TaskMutex::new(memory),
            before_restore: std::sync::Mutex::new(None),
            before_revoke: std::sync::Mutex::new(None),
            after_revoke: std::sync::Mutex::new(None),
            before_drop: std::sync::Mutex::new(None),
        }
    }
}
impl Drop for SwapMemory {
    fn drop(&mut self) {
        if let Some(hook) = self.before_drop.lock().unwrap().take() {
            hook(&self.memory.lock().unwrap());
        }
    }
}
impl crate::memory::shared_file::MemoryMappingOwner for SwapMemory {
    fn revoke_swappable_page(
        &self,
        page: &crate::memory::SwappablePage,
        ppn: crate::memory::PhysicalPageNumber,
    ) -> Option<usize> {
        let before = self.before_revoke.lock().unwrap().take();
        if let Some(before) = before {
            before();
        }
        let result = self
            .memory
            .try_lock()
            .map(|mut memory| memory.revoke_swappable_page(page, ppn));
        let after = self.after_revoke.lock().unwrap().take();
        if let Some(after) = after {
            after();
        }
        result
    }
    fn revoke_shared_file_writers(
        &self,
        id: SharedFileId,
        _: &mut crate::execution::sync::TaskMutexWaitPreparation,
    ) {
        self.memory.lock().unwrap().revoke_shared_file_writers(id);
    }
    fn invalidate_cached_file(
        &self,
        id: SharedFileId,
        first: u64,
        last: u64,
        _: &mut crate::execution::sync::TaskMutexWaitPreparation,
    ) {
        self.memory
            .lock()
            .unwrap()
            .invalidate_cached_file(id, first, last);
    }
    fn invalidate_shared_file(
        &self,
        _: SharedFileId,
        _: u64,
        _: &mut crate::execution::sync::TaskMutexWaitPreparation,
    ) {
        panic!("file invalidation outside swap fixture");
    }
    fn restore_swapped_pages(
        &self,
        identity: SharedFileId,
        _: &mut crate::execution::sync::TaskMutexWaitPreparation,
    ) -> Result<(), crate::memory::SharedFileError> {
        let action = self.before_restore.lock().unwrap().take();
        if let Some(action) = action {
            action();
        }
        self.memory
            .lock()
            .unwrap()
            .restore_swapped_pages(identity)
            .map_err(|error| {
                if error.is_out_of_memory() {
                    crate::memory::SharedFileError::OutOfMemory
                } else {
                    crate::memory::SharedFileError::Io
                }
            })
    }
}
impl crate::memory::shared_file::MemoryReclaimer for SwapMemory {
    fn reclaim_pages(&self, request: ReclaimRequest) -> ReclaimResult {
        background_reclaim::reclaim_address_space(&self.memory, request)
    }
}
fn swap_memory(no_access: bool, count: usize) -> MemorySet {
    let mut memory = MemorySet::empty();
    let start = 0x5_0401 * config::PAGE_SIZE;
    let permission = if no_access {
        MapPermission::U
    } else {
        MapPermission::U | MapPermission::R | MapPermission::W
    };
    let mut area = MapArea::anonymous(
        start.into(),
        (start + count * config::PAGE_SIZE).into(),
        permission,
    );
    for index in 0..count {
        let mut frame = alloc().unwrap();
        frame.bytes_mut().fill((index as u8).wrapping_add(0x51));
        let ppn = frame.ppn;
        let vpn = VirtualPageNumber::from_vpn(start / config::PAGE_SIZE + index);
        area.data_frames
            .try_insert(vpn, PrivateResident::user(frame).unwrap())
            .unwrap();
        if no_access {
            memory.page_table.reserve(vpn).unwrap();
        } else {
            memory
                .page_table
                .map(vpn, ppn, permission.into(), &mut TranslationCommit::new())
                .unwrap();
        }
    }
    memory.commit_area(FallibleMap::try_prepare(area.vpn_range.start, area).unwrap());
    memory
}
#[test]
#[ignore = "diagnostic CPU measurement; run alone in release"]
fn production_swap_slot_cpu_cost() {
    let _serial = SWAP_TEST.lock().unwrap();
    use crate::memory::swap;
    use std::{hint::black_box, time::Instant};
    fn measure(name: &str, mut operation: impl FnMut()) {
        const ITERATIONS: usize = 100_000;
        for _ in 0..ITERATIONS {
            operation();
        }
        let mut samples = std::array::from_fn::<_, 5, _>(|_| {
            let start = Instant::now();
            for _ in 0..ITERATIONS {
                operation();
            }
            start.elapsed()
        });
        samples.sort_unstable();
        println!(
            "{name}: {:.2} ns/op",
            samples[2].as_nanos() as f64 / ITERATIONS as f64
        );
    }
    let disk = SwapDisk::new(9);
    swap::activate(disk, b"/dev/test", 0x8005).unwrap();
    measure("swap one-area reserve/retire", || {
        drop(black_box(swap::reserve().unwrap()));
    });
    let slot = swap::reserve().unwrap();
    measure("swap slot clone/retire", || {
        drop(black_box(slot.clone()));
    });
    drop(slot);
    for identifier in 2..=32 {
        swap::activate(SwapDisk::with_identity(9, identifier), b"/dev/test", 0x8005).unwrap();
    }
    measure("swap 32-area reserve/retire", || {
        drop(black_box(swap::reserve().unwrap()));
    });
    measure("swap 32-area statistics", || {
        black_box(swap::statistics());
    });
    let mut wait = crate::execution::sync::TaskMutexWaitPreparation;
    for inode in 1..=32 {
        swap::deactivate(
            SharedFileId {
                filesystem: 999,
                inode,
            },
            &mut wait,
        )
        .unwrap();
    }
    assert_eq!(swap::statistics(), (0, 0));
}
#[test]
fn production_swap_reclaim_fork_io_failure_and_drain_keep_one_page_owner() {
    let _serial = SWAP_TEST.lock().unwrap();
    use crate::memory::{shared_file, swap};
    use core::sync::atomic::Ordering;
    for no_access in [false, true] {
        let disk = SwapDisk::new(9);
        swap::activate(disk.clone(), b"/dev/test", 0x8005).unwrap();
        assert_eq!(swap::statistics(), (7, 7));
        // child 先占低 slot，swapoff 首次扫描它时为空。
        let child = Arc::new(SwapMemory::new(MemorySet::empty()));
        shared_file::register_memory_mapping_owner(child.clone()).unwrap();
        let parent = Arc::new(SwapMemory::new(swap_memory(no_access, 3)));
        shared_file::register_memory_mapping_owner(parent.clone()).unwrap();
        shared_file::register_memory_reclaimer(parent.clone()).unwrap();
        let vpn = VirtualPageNumber::from_vpn(0x5_0401);
        let _original = parent
            .memory
            .lock()
            .unwrap()
            .page_table
            .translate(vpn)
            .map(|leaf| (leaf.ppn(), leaf.permissions()));
        assert_eq!(
            shared_file::reclaim_pages(1).reclaimed_pages(),
            0,
            "direct allocator reclaim cannot swap"
        );
        assert_eq!(swap::statistics(), (7, 7));
        disk.write_error.store(true, Ordering::Relaxed);
        let mut request = background_request();
        request.limit_scans(1);
        assert_eq!(parent.reclaim_pages(request).reclaimed_pages(), 0);
        assert_eq!(swap::statistics(), (7, 7));
        assert_eq!(
            parent
                .memory
                .lock()
                .unwrap()
                .page_table
                .translate(vpn)
                .map(|leaf| (leaf.ppn(), leaf.permissions())),
            None
        );
        assert_eq!(
            parent
                .memory
                .lock()
                .unwrap()
                .areas
                .first_key_value()
                .unwrap()
                .1
                .data_frames
                .get(&vpn)
                .unwrap()
                .content_snapshot()
                .unwrap()
                .bytes(),
            &[0x51; 4096]
        );
        disk.write_error.store(false, Ordering::Relaxed);
        assert_eq!(
            shared_file::reclaim_background_pages(1).reclaimed_pages(),
            1
        );
        assert_eq!(swap::statistics(), (7, 6));
        let selected = parent
            .memory
            .lock()
            .unwrap()
            .areas
            .first_key_value()
            .unwrap()
            .1
            .data_frames
            .iter()
            .find_map(|(&vpn, page)| (!page.is_installed()).then_some(vpn))
            .unwrap();
        assert!(
            parent
                .memory
                .lock()
                .unwrap()
                .page_table
                .translate(selected)
                .is_none()
        );
        disk.read_error.store(true, Ordering::Relaxed);
        let mut wait = crate::execution::sync::TaskMutexWaitPreparation;
        assert_eq!(
            swap::deactivate(
                SharedFileId {
                    filesystem: 999,
                    inode: 1
                },
                &mut wait
            ),
            Err(swap::SwapError::Io)
        );
        assert_eq!(swap::statistics(), (7, 6));
        assert!(
            !disk.closed.load(Ordering::Acquire),
            "failed swapoff must keep backing lease"
        );
        disk.read_error.store(false, Ordering::Relaxed);
        let parent_ref = Arc::downgrade(&parent);
        let child_ref = child.clone();
        *parent.before_restore.lock().unwrap() = Some(Box::new(move || {
            // 生产 registry 已扫空 child；parent callback 前执行生产 COW，模拟
            // fork 先持 parent guard 而 truncate/drain callback 随后取得该 guard。
            let parent = parent_ref.upgrade().unwrap();
            let mut parent_mm = parent.memory.lock().unwrap();
            let mut child_mm = child_ref.memory.lock().unwrap();
            parent_mm.try_clone_for_fork(&mut child_mm).unwrap();
            assert_eq!(
                swap::statistics(),
                (7, 6),
                "fork duplicates slot reference, not slot allocation"
            );
            assert!(
                child_mm
                    .areas
                    .first_key_value()
                    .unwrap()
                    .1
                    .data_frames
                    .get(&selected)
                    .unwrap()
                    .content_snapshot()
                    .is_none()
            );
            shared_file::complete_memory_mapping_owner_initialization();
        }));
        swap::deactivate(
            SharedFileId {
                filesystem: 999,
                inode: 1,
            },
            &mut wait,
        )
        .unwrap();
        assert_eq!(swap::statistics(), (0, 0));
        assert!(
            disk.closed.load(Ordering::Acquire),
            "close must precede return despite surviving snapshot Arc"
        );
        for owner in [&parent, &child] {
            let memory = owner.memory.lock().unwrap();
            assert_eq!(memory.page_table.translate(selected).is_some(), !no_access);
            let page = memory
                .areas
                .first_key_value()
                .unwrap()
                .1
                .data_frames
                .get(&selected)
                .unwrap();
            assert_eq!(
                page.content_snapshot().unwrap().bytes(),
                &[(selected.as_usize() as u8)
                    .wrapping_sub(1)
                    .wrapping_add(0x51); 4096]
            );
        }
        assert!(
            Arc::ptr_eq(
                parent
                    .memory
                    .lock()
                    .unwrap()
                    .areas
                    .first_key_value()
                    .unwrap()
                    .1
                    .data_frames
                    .get(&selected)
                    .unwrap()
                    .content_snapshot()
                    .unwrap()
                    .owner,
                child
                    .memory
                    .lock()
                    .unwrap()
                    .areas
                    .first_key_value()
                    .unwrap()
                    .1
                    .data_frames
                    .get(&selected)
                    .unwrap()
                    .content_snapshot()
                    .unwrap()
                    .owner
            ),
            "fork aliases share canonical content and retain COW protection"
        );
    }
    // 256 records/256 budget：统一轮转不能让同一半永久只通过 clean 筛选。
    let disk = SwapDisk::new(514);
    swap::activate(disk.clone(), b"/dev/test", 0x8005).unwrap();
    let owner = Arc::new(SwapMemory::new(swap_memory(false, 256)));
    shared_file::register_memory_mapping_owner(owner.clone()).unwrap();
    shared_file::register_memory_reclaimer(owner.clone()).unwrap();
    for _ in 0..4 {
        let result = shared_file::reclaim_background_pages(256);
        assert!(result.scanned_pages() <= 256);
    }
    assert!(
        owner
            .memory
            .lock()
            .unwrap()
            .areas
            .first_key_value()
            .unwrap()
            .1
            .data_frames
            .values()
            .all(|page| page.content_snapshot().is_none()),
        "every half must eventually accept swap candidates"
    );
    let mut wait = crate::execution::sync::TaskMutexWaitPreparation;
    swap::deactivate(
        SharedFileId {
            filesystem: 999,
            inode: 1,
        },
        &mut wait,
    )
    .unwrap();
    assert!(
        owner
            .memory
            .lock()
            .unwrap()
            .areas
            .first_key_value()
            .unwrap()
            .1
            .data_frames
            .values()
            .all(|page| page.content_snapshot().is_some())
    );
    // 已退出 mm 的 Weak 无法升级，但最后一个字段仍持槽：drain 等待 retirement。
    let disk = SwapDisk::new(9);
    swap::activate(disk.clone(), b"/dev/test", 0).unwrap();
    let orphan = swap::reserve().unwrap();
    let (finished, completion) = std::sync::mpsc::channel();
    let drain = std::thread::spawn(move || {
        let mut wait = crate::execution::sync::TaskMutexWaitPreparation;
        let result = swap::deactivate(
            SharedFileId {
                filesystem: 999,
                inode: 1,
            },
            &mut wait,
        );
        finished.send(result).unwrap();
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while swap::reserve().is_some() {
        assert!(
            std::time::Instant::now() < deadline,
            "drain did not enter Draining"
        );
        std::thread::yield_now();
    }
    assert!(!disk.closed.load(Ordering::Acquire));
    assert!(completion.try_recv().is_err(), "live slot must delay close");
    drop(orphan);
    assert_eq!(
        completion
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap(),
        Ok(())
    );
    drain.join().unwrap();
    assert!(disk.closed.load(Ordering::Acquire));
    assert_eq!(swap::statistics(), (0, 0));
}

fn background_request() -> ReclaimRequest {
    struct Capture(std::sync::Mutex<Option<ReclaimRequest>>);
    impl crate::memory::MemoryReclaimer for Capture {
        fn reclaim_pages(&self, request: ReclaimRequest) -> ReclaimResult {
            *self.0.lock().unwrap() = Some(request);
            ReclaimResult::new(0, 0)
        }
    }
    let capture = Arc::new(Capture(std::sync::Mutex::new(None)));
    crate::memory::register_memory_reclaimer(capture.clone()).unwrap();
    let _ = shared_file::reclaim_background_pages(1);
    capture
        .0
        .lock()
        .unwrap()
        .take()
        .expect("real background capability")
}

include!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fork_file_eof/retirement.rs"
));

include!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fork_file_eof/memory_file.rs"
));

include!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fork_file_eof/shared_anonymous.rs"
));

include!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fork_file_eof/migration_order.rs"
));

include!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fork_file_eof/page_cost.rs"
));

include!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fork_file_eof/interrupt.rs"
));
