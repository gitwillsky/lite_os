#![feature(allocator_ext)]
//! 执行生产 fork/COW、VMA/backing 与通用/Sv39 页表；替换物理分配、CPU fence 和非目标 device/trampoline 装配。
#![allow(dead_code)]

extern crate alloc;

#[path = "../../../kernel/src/sync"]
mod sync {
    mod wait_completion;
    pub(crate) use wait_completion::WaitCompletion;
    mod task_event;
    mod task_wait;
    pub(crate) use task_event::TaskEvent;
    pub(crate) use task_wait::{
        TaskWaitKey, TaskWaitTarget, WaitResult, install_wait_target_factory, task_interrupted,
    };
    pub(crate) struct TaskMutexWaitPreparation;
    impl TaskMutexWaitPreparation {
        pub(crate) fn prepare() -> Result<Self, ()> {
            Ok(Self)
        }
    }
    pub(crate) struct IrqMutex<T>(spin::Mutex<T>);
    impl<T> IrqMutex<T> {
        pub(crate) const fn new(value: T) -> Self {
            Self(spin::Mutex::new(value))
        }
        pub(crate) fn lock(&self) -> spin::MutexGuard<'_, T> {
            self.0.lock()
        }
    }
    pub(crate) type TaskMutexGuard<'a, T> = std::sync::MutexGuard<'a, T>;
    pub(crate) struct TaskMutex<T>(std::sync::Mutex<T>);
    impl<T> TaskMutex<T> {
        pub(crate) fn new(value: T) -> Self {
            Self(std::sync::Mutex::new(value))
        }
        pub(crate) fn try_lock(&self) -> Option<TaskMutexGuard<'_, T>> {
            self.0.try_lock().ok()
        }
        pub(crate) fn lock(&self) -> Result<std::sync::MutexGuard<'_, T>, ()> {
            self.0.lock().map_err(|_| ())
        }
    }
}

#[path = "../../../kernel/src/fallible_tree/mod.rs"]
mod fallible_tree;

#[path = "../../../kernel/src/arch/riscv64"]
mod arch {
    mod page_table;
    mod pte;
    mod sv39;
    pub(crate) mod user {
        pub(crate) const ELF_MACHINE: u16 = 243;
        pub(crate) fn valid_elf_flags(_: u32) -> bool {
            true
        }
    }
    pub(crate) mod mmu {
        include!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/riscv_memory_mmu_fixture.rs"
        ));
    }
}

#[path = "../../../kernel/src/memory"]
mod memory {
    pub(crate) mod config {
        pub(crate) const PAGE_SIZE: usize = 4096;
        pub(crate) const USER_ADDRESS_END: usize = 1 << 38;
        pub(crate) const USER_STACK_SIZE: usize = 8192;
    }
    mod address;
    mod executable;
    pub(crate) mod page_table;
    mod permissions;
    mod retire;
    mod shared_file;
    mod shared_memory;
    pub(crate) use mm::MemoryError;
    pub(crate) use mm::SharedMemoryBacking;
    pub(crate) use shared_memory::{SharedMemoryMappingSource, SharedMemoryVma};
    mod swappable_page;
    pub(crate) use address::PhysicalPageNumber;
    pub(crate) use config::PAGE_SIZE;
    pub(crate) use shared_file::{
        MappedFileOwner, MemoryReclaimer, ReclaimRequest, ReclaimResult, SharedFrame,
        SharedPagePin, invalidate_shared_file, register_memory_reclaimer,
    };
    pub(crate) use swappable_page::SwappablePage;
    pub(crate) mod swap;
    pub(crate) use executable::ExecutableSource;
    pub(crate) use shared_file::{SharedFileError, SharedFileId, SharedFileMapping, SharedPage};
    pub(crate) mod frame_allocator {
        use super::address::PhysicalPageNumber;
        use std::alloc::{Layout, alloc_zeroed, dealloc};
        #[derive(Debug)]
        pub(crate) struct FrameTracker {
            pub(crate) ppn: PhysicalPageNumber,
        }
        impl FrameTracker {
            pub(crate) fn bytes(&self) -> &[u8] {
                // SAFETY: ppn 的唯一 owner 保活一个完整、初始化的 4096-byte allocation。
                unsafe { core::slice::from_raw_parts(self.ppn.as_page_ptr(), 4096) }
            }
            pub(crate) fn bytes_mut(&mut self) -> &mut [u8] {
                // SAFETY: &mut owner 证明该 allocation 的独占访问。
                unsafe { core::slice::from_raw_parts_mut(self.ppn.as_page_mut_ptr(), 4096) }
            }
        }
        impl Drop for FrameTracker {
            fn drop(&mut self) {
                // SAFETY: 原地址与相同 layout 交还分配器。
                unsafe {
                    dealloc(
                        self.ppn.as_page_mut_ptr(),
                        Layout::from_size_align(4096, 4096).unwrap(),
                    )
                }
            }
        }
        std::thread_local! { pub(crate) static FAIL_NEXT_FRAME: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }
        pub(crate) fn alloc() -> Option<FrameTracker> {
            if FAIL_NEXT_FRAME.with(|flag| flag.replace(false)) {
                return None;
            }
            // SAFETY: 固定有效非零 layout；null 作为真实物理分配失败返回。
            let pointer = unsafe { alloc_zeroed(Layout::from_size_align(4096, 4096).unwrap()) };
            (!pointer.is_null()).then(|| FrameTracker {
                ppn: (pointer as usize >> 12).into(),
            })
        }
        pub(crate) fn alloc_copy(bytes: &[u8]) -> Option<FrameTracker> {
            let mut frame = alloc()?;
            frame.bytes_mut().copy_from_slice(bytes);
            Some(frame)
        }
    }
    #[path = "mm"]
    pub(crate) mod mm {
        use super::config;
        use super::retire::{PrivateReclaimWalk, reclaim_release_decision, revoke_and_synchronize};
        use super::shared_file::{ReclaimRequest, ReclaimResult};
        type TranslationSynchronizationError = ();
        use super::{
            address::{PhysicalAddress, VirtualAddress, VirtualPageNumber},
            frame_allocator::{FrameTracker, alloc},
            page_table::{PagePermissions, PageTable},
            permissions::MapPermission,
            shared_file::{SharedFileError, SharedFileId, SharedFileMapping, SharedPage},
        };
        use crate::fallible_tree::{FallibleMap, VacantEntry};
        use alloc::sync::Arc;
        use core::ops::Range;
        mod area;
        mod area_publication;
        mod background_reclaim;
        mod cow;
        mod error;
        mod file_page_range;
        mod private_area;
        mod private_fault;
        mod private_reclaim;
        mod resident;
        mod retire;
        mod shared_area {
            include!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../kernel/src/memory/mm/shared_area.rs"
            ));
            // 显式 host 观察 seam；不扩大 production owner 的 private 接口。
            pub(super) fn statistics_observer(
                backing: &AnonymousSharedBacking,
            ) -> Arc<crate::memory::shared_memory::SharedPageStatistics> {
                backing.statistics.clone()
            }
        }
        pub(crate) use shared_area::AnonymousSharedBacking as SharedMemoryBacking;
        mod shared_swap;
        #[path = "mmap/shared_write.rs"]
        mod shared_write;
        mod statistics;
        mod thread_context;
        mod writer_tests {
            include!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fork_file_eof/shared_write.rs"
            ));
        }
        mod swap;
        mod vma_index_state;
        use area::{MapArea, MapType, VmaKind};
        pub(crate) use error::MemoryError;
        use error::try_memory_arc;
        use file_page_range::FilePageRange;
        use private_area::PrivateFileArea;
        use resident::{PrivateBacking, PrivateResident};
        use shared_area::{SharedAnonymousArea, SharedFileArea, SharedResident};
        use shootdown::{TranslationCommit, revoke_and_commit};
        use vma_index_state::{VmaContribution, VmaIndexState};

        #[derive(Clone, Copy, PartialEq, Eq)]
        enum PageFaultAccess {
            Read,
            Write,
            Execute,
        }

        pub(crate) mod shootdown {
            #[derive(Clone, Copy, Debug)]
            pub(crate) enum TranslationTransition {
                Publish,
                Relax,
                Revoke,
            }
            pub(crate) struct TranslationCommit {
                retired: crate::fallible_tree::FallibleMap<
                    usize,
                    crate::memory::frame_allocator::FrameTracker,
                >,
            }
            impl Drop for TranslationCommit {
                fn drop(&mut self) {
                    assert!(
                        self.retired.is_empty(),
                        "unsynchronized translation retirement"
                    );
                }
            }
            impl TranslationCommit {
                pub(crate) fn stale_fault(_: usize) -> Self {
                    Self {
                        retired: crate::fallible_tree::FallibleMap::new(),
                    }
                }
                pub(crate) fn new() -> Self {
                    Self {
                        retired: crate::fallible_tree::FallibleMap::new(),
                    }
                }
                pub(crate) fn record(&mut self, _: usize, _: TranslationTransition) {
                    // CPU fence mechanism is external to this host fixture.
                }
                pub(crate) fn record_range(
                    &mut self,
                    _: usize,
                    _: usize,
                    _: TranslationTransition,
                ) {
                }
                pub(crate) fn record_instruction_publication(&mut self, _: usize, _: usize) {}
                pub(crate) fn retain_table_pages(
                    &mut self,
                    pages: impl IntoIterator<
                        Item = crate::fallible_tree::VacantEntry<
                            usize,
                            crate::memory::frame_allocator::FrameTracker,
                        >,
                    >,
                ) {
                    for page in pages {
                        self.retired.commit_vacant(page);
                    }
                }
                pub(crate) fn synchronize(&mut self) -> Result<(), ()> {
                    self.retired.clear();
                    Ok(())
                }
                pub(crate) fn finish_unpublished(mut self) {
                    self.retired.clear();
                }
            }
            pub(super) fn revoke_and_commit<T>(
                owner: T,
                revoke: impl FnOnce(&mut T, &mut TranslationCommit),
            ) -> Result<T, ()> {
                let commit = core::cell::RefCell::new(TranslationCommit::new());
                crate::memory::retire::revoke_and_synchronize(
                    owner,
                    |owner| revoke(owner, &mut commit.borrow_mut()),
                    |_| commit.borrow_mut().synchronize(),
                )
            }
        }
        // 本 fixture 不建立 device VMA；这些 seam 若意外命中即失败。
        #[derive(Debug, Clone)]
        struct DeviceArea;
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        enum DevicePageAccounting {
            InsertedPages,
        }
        impl DeviceArea {
            fn partition(
                _: Option<Self>,
                _: VirtualPageNumber,
                _: VirtualPageNumber,
                _: VirtualPageNumber,
            ) -> (Option<Self>, Option<Self>, Option<Self>) {
                (None, None, None)
            }
        }
        impl MapArea {
            fn map_device_area(
                &self,
                _: &mut PageTable,
                _: &mut TranslationCommit,
            ) -> Result<(), MemoryError> {
                panic!("device VMA outside fixture")
            }
            fn try_clone_device_into(
                &self,
                _: &mut PageTable,
                _: &mut TranslationCommit,
            ) -> Result<Self, MemoryError> {
                panic!("device fork outside fixture")
            }
            fn unmap_device_area(&self, _: &mut PageTable, _: &mut TranslationCommit) {
                panic!("device unmap outside fixture")
            }
        }
        struct MemorySet {
            page_table: PageTable,
            areas: FallibleMap<VirtualPageNumber, MapArea>,
            executable_owner: Option<Arc<dyn crate::memory::MappedFileOwner>>,
            thread_context_hint: Option<usize>,
            vma_index_state: VmaIndexState,
            code_range: Range<usize>,
            program_break: Option<usize>,
            argument_range: Range<usize>,
            private_reclaim_cursor: VirtualPageNumber,
            private_reclaim_exclusion: Option<Range<VirtualPageNumber>>,
        }
        impl MemorySet {
            fn empty() -> Self {
                Self {
                    page_table: PageTable::try_new(crate::arch::mmu::AddressSpaceKind::User)
                        .unwrap(),
                    areas: FallibleMap::new(),
                    executable_owner: None,
                    thread_context_hint: None,
                    vma_index_state: VmaIndexState::new(),
                    code_range: 0..0,
                    program_break: None,
                    argument_range: 0..0,
                    private_reclaim_cursor: VirtualPageNumber::from_vpn(0),
                    private_reclaim_exclusion: None,
                }
            }
            fn map_trampoline(&mut self) -> Result<(), MemoryError> {
                Ok(())
            }
        }
        #[derive(Debug)]
        struct Backing {
            size: core::sync::atomic::AtomicU64,
            writable: core::sync::atomic::AtomicBool,
        }
        impl SharedFileMapping for Backing {
            fn writable(&self) -> bool {
                self.writable.load(core::sync::atomic::Ordering::Acquire)
            }
            fn id(&self) -> SharedFileId {
                SharedFileId {
                    filesystem: 1,
                    inode: 1,
                }
            }
            fn size(&self) -> u64 {
                self.size.load(core::sync::atomic::Ordering::Acquire)
            }
            fn page(&self, _: u64) -> Result<Arc<dyn SharedPage>, super::SharedFileError> {
                panic!("fork must not fault file pages")
            }
            fn sync_range(&self, _: u64, _: u64) -> Result<(), super::SharedFileError> {
                panic!("fork must not write file pages")
            }
        }
        #[derive(Debug)]
        struct Page {
            frame: super::shared_file::SharedFrame,
            writers: core::sync::atomic::AtomicUsize,
        }
        impl SharedPage for Page {
            fn pin(&self) -> Result<super::SharedPagePin<'_>, SharedFileError> {
                Ok(super::SharedPagePin::Cached(&self.frame))
            }
            fn acquire_writer(&self) {
                self.writers
                    .fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            }
            fn release_writer(&self) {
                assert!(
                    self.writers
                        .fetch_sub(1, core::sync::atomic::Ordering::Relaxed)
                        > 0
                );
            }
        }
        fn populate(
            shared: bool,
            no_access: bool,
            offset: usize,
        ) -> (MemorySet, Arc<Backing>, Vec<Arc<Page>>) {
            let mut memory = MemorySet::empty();
            let start = 0x4_0201 * config::PAGE_SIZE;
            let range = FilePageRange::new(offset as u64, 3 * config::PAGE_SIZE).unwrap();
            let backing = Arc::new(Backing {
                size: core::sync::atomic::AtomicU64::new((offset + 3 * config::PAGE_SIZE) as u64),
                writable: core::sync::atomic::AtomicBool::new(true),
            });
            let permission = if no_access {
                MapPermission::U
            } else {
                MapPermission::U | MapPermission::R | MapPermission::W
            };
            let mut area = if shared {
                MapArea::shared_file(
                    start.into(),
                    (start + 3 * config::PAGE_SIZE).into(),
                    permission,
                    backing.clone(),
                    range,
                )
            } else {
                MapArea::file(
                    start.into(),
                    (start + 3 * config::PAGE_SIZE).into(),
                    permission,
                    PrivateFileArea::cached_file(backing.clone(), start, range),
                )
            };
            let mut pages = Vec::new();
            for index in 0..3 {
                let vpn = VirtualPageNumber::from_vpn(start / config::PAGE_SIZE + index);
                let ppn = if shared {
                    let page = Arc::new(Page {
                        frame: super::shared_file::SharedFrame::allocate().unwrap(),
                        writers: core::sync::atomic::AtomicUsize::new(0),
                    });
                    let ppn = page.frame.ppn();
                    area.shared_file
                        .as_mut()
                        .unwrap()
                        .resident
                        .try_insert(
                            vpn,
                            SharedResident::new(
                                &(page.clone() as Arc<dyn SharedPage>),
                                !no_access,
                                &super::SharedPagePin::Cached(&page.frame),
                            ),
                        )
                        .unwrap();
                    pages.push(page);
                    ppn
                } else {
                    let mut frame = alloc().unwrap();
                    frame.bytes_mut().fill(0x50 + index as u8);
                    let ppn = frame.ppn;
                    let mut resident = PrivateResident::user(frame).unwrap();
                    resident.dirty = index != 0;
                    resident.discardable = index == 1;
                    area.data_frames.try_insert(vpn, resident).unwrap();
                    ppn
                };
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
            (memory, backing, pages)
        }
        mod swap_tests {
            use super::*;
            use crate::memory::shared_file;
            use crate::memory::shared_file::MemoryReclaimer;
            include!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fork_file_eof/swap.rs"
            ));
        }
        mod thread_context_tests {
            use super::*;
            include!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fork_file_eof/thread_context.rs"
            ));
        }
        mod observations {
            use super::*;
            include!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fork_file_eof/observations.rs"
            ));
        }
        #[test]
        fn fork_filters_published_eof_before_inheriting_records_and_translations() {
            // 重现 truncate 已公布 EOF、callback 等待 parent mm 的确定状态。
            // 对 append/已扫 tombstone 漏扫的 child，生产 fork 必须自行阻止旧 resident 逃逸。
            for shared in [false, true] {
                for no_access in [false, true] {
                    for offset in [0, 2 * config::PAGE_SIZE] {
                        let (mut parent, backing, pages) = populate(shared, no_access, offset);
                        backing.size.store(
                            (offset + config::PAGE_SIZE + 17) as u64,
                            core::sync::atomic::Ordering::Release,
                        );
                        let mut child = MemorySet::empty();
                        parent.try_clone_for_fork(&mut child).unwrap();
                        let (_, parent_area) = parent.areas.first_key_value().unwrap();
                        let (_, child_area) = child.areas.first_key_value().unwrap();
                        assert_eq!(child_area.vpn_range, parent_area.vpn_range);
                        assert_eq!(child_area.map_permission, parent_area.map_permission);
                        assert_eq!(child.vma_index_state, parent.vma_index_state);
                        let start = child_area.vpn_range.start.as_usize();
                        let resident_count = if shared {
                            child_area.shared_file.as_ref().unwrap().resident.len()
                        } else {
                            child_area.data_frames.len()
                        };
                        assert_eq!(
                            resident_count, 2,
                            "partial EOF page must remain, whole EOF page must disappear"
                        );
                        for index in 0..3 {
                            let vpn = VirtualPageNumber::from_vpn(start + index);
                            let leaf = child.page_table.translate(vpn);
                            assert_eq!(leaf.is_some(), index < 2 && !no_access);
                            if !shared && index < 2 {
                                let source = parent_area.data_frames.get(&vpn).unwrap();
                                let target = child_area.data_frames.get(&vpn).unwrap();
                                assert!(Arc::ptr_eq(
                                    source.content_snapshot().unwrap().owner,
                                    target.content_snapshot().unwrap().owner
                                ));
                                assert_eq!(
                                    (target.dirty, target.discardable),
                                    (source.dirty, source.discardable)
                                );
                                assert_eq!(
                                    target.content_snapshot().unwrap().bytes()[0],
                                    0x50 + index as u8
                                );
                                if let Some(leaf) = leaf {
                                    assert!(!leaf.permissions().contains(PagePermissions::WRITE));
                                }
                            }
                        }
                        if shared {
                            assert_eq!(
                                child_area.shared_file.as_ref().unwrap().pages,
                                parent_area.shared_file.as_ref().unwrap().pages
                            );
                            for (index, page) in pages.iter().enumerate() {
                                assert_eq!(
                                    page.writers.load(core::sync::atomic::Ordering::Relaxed),
                                    if no_access {
                                        0
                                    } else if index < 2 {
                                        2
                                    } else {
                                        1
                                    }
                                );
                            }
                            drop(child);
                            for page in &pages {
                                assert_eq!(
                                    page.writers.load(core::sync::atomic::Ordering::Relaxed),
                                    usize::from(!no_access)
                                );
                            }
                        }
                        // parent 仍待 truncate callback；fork 不能以清 parent 的方式掩盖窗口。
                        assert_eq!(
                            if shared {
                                parent_area.shared_file.as_ref().unwrap().resident.len()
                            } else {
                                parent_area.data_frames.len()
                            },
                            3
                        );
                    }
                }
            }
        }
    }
}

mod timer {
    pub(crate) fn get_realtime_ns() -> u64 {
        1_000_000_000
    }
}

#[path = "../../../kernel/src/fs"]
mod fs {
    #[derive(Debug, PartialEq, Eq)]
    pub(crate) enum FileSystemError {
        InvalidOperation,
        OutOfMemory,
        NoSpace,
        IoError,
        PermissionDenied,
        Busy,
    }
    mod storage_budget;
    use storage_budget::StorageBudget;
    mod inode_times;
    mod memory_file;
    use alloc::sync::Arc;

    // Production file/quota owners; exposes observations, never their private mutable state.
    pub(crate) struct QuotaFile {
        pub(crate) file: Arc<memory_file::MemoryFile>,
        budget: Arc<StorageBudget>,
    }
    impl QuotaFile {
        pub(crate) fn new(pages: u64) -> Self {
            Self::new_with_sealing(pages, false)
        }
        pub(crate) fn new_with_sealing(pages: u64, allow_sealing: bool) -> Self {
            let budget = StorageBudget::new(Some(pages), None).unwrap();
            let file = memory_file::MemoryFile::new(
                crate::memory::SharedFileId {
                    filesystem: 998,
                    inode: 1,
                },
                budget.clone(),
                allow_sealing,
            )
            .unwrap();
            Self { file, budget }
        }
        pub(crate) fn truncate(&self, size: u64) -> Result<(), FileSystemError> {
            self.file.truncate_with_metadata(size, || Ok(()), |_| ())
        }
        pub(crate) fn truncate_transaction<G>(
            &self,
            size: u64,
            prepare: impl FnOnce() -> Result<G, FileSystemError>,
            commit: impl FnOnce(&mut G),
        ) -> Result<(), FileSystemError> {
            self.file.truncate_with_metadata(size, prepare, commit)
        }
        pub(crate) fn set_modified(&self, seconds: u64) {
            let _writer = self.file.begin_write().unwrap();
            inode_times::TimeOwner::Memory(self.file.times()).set(None, Some(seconds));
        }
        pub(crate) fn times(&self) -> (u64, u64, u64) {
            let times = self.file.times().snapshot();
            (times.atime, times.mtime, times.ctime)
        }
        pub(crate) fn used_pages(&self) -> u64 {
            self.budget.snapshot().used_pages
        }
    }
}

#[path = "../src/interruptible_event_target.rs"]
mod interruptible_event_target;

#[test]
fn write_seal_waits_for_the_last_shared_mapping_lease() {
    let quota = fs::QuotaFile::new_with_sealing(2, true);
    let writable = quota.file.mapping(true, true, false).unwrap();
    let fork_alias = writable.clone();
    assert_eq!(quota.file.add_seals(8), Err(fs::FileSystemError::Busy));
    assert_eq!(quota.file.seals().unwrap() & 8, 0);
    drop(writable);
    assert_eq!(quota.file.add_seals(8), Err(fs::FileSystemError::Busy));
    drop(fork_alias);
    let private = quota.file.mapping(false, true, true).unwrap();
    quota.file.add_seals(8).unwrap();
    assert!(private.may_write());
    assert_eq!(
        quota.file.mapping(true, true, true).unwrap_err(),
        fs::FileSystemError::PermissionDenied
    );
    assert!(!quota.file.mapping(true, true, false).unwrap().may_write());
}

#[test]
fn future_write_seal_preserves_existing_admission_and_blocks_new_writers() {
    let quota = fs::QuotaFile::new_with_sealing(2, true);
    let existing = quota.file.mapping(true, true, true).unwrap();
    quota.file.add_seals(16).unwrap();
    assert!(existing.may_write());
    assert_eq!(
        quota.file.mapping(true, true, true).unwrap_err(),
        fs::FileSystemError::PermissionDenied
    );
    let read_only = quota.file.mapping(true, true, false).unwrap();
    assert!(!read_only.may_write());
    drop(existing);
    quota.file.add_seals(8).unwrap();
}
