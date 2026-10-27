#![feature(allocator_ext)]
//! 执行生产 private fault、resident、通用页表与 Sv39 walker；仅替换物理分配和 CPU fence。
#![allow(dead_code)]

extern crate alloc;

use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::{Cell, RefCell},
};

thread_local! {
    static REQUIRE_RECLAIM: Cell<bool> = const { Cell::new(false) };
    static FRAME_FREED: Cell<bool> = const { Cell::new(false) };
    static REJECT_ALLOCATION: Cell<bool> = const { Cell::new(false) };
    static REJECT_AFTER_REVOKE: Cell<bool> = const { Cell::new(false) };
    static EVENTS: RefCell<Vec<(usize, memory::mm::shootdown::TranslationTransition)>> = const { RefCell::new(Vec::new()) };
}

struct FaultAllocator;
// SAFETY: 成功 allocation/deallocation 完整委托 System；线程局部故障只返回 null。
unsafe impl GlobalAlloc for FaultAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if REJECT_ALLOCATION.try_with(Cell::get).unwrap_or(false) {
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

#[path = "../../../kernel/src/sync"]
mod sync {
    mod wait_completion;
    pub(crate) use wait_completion::WaitCompletion;
    mod task_event;
    mod task_wait;
    pub(crate) use task_event::TaskEvent;
    pub(crate) use task_wait::{WaitResult, task_interrupted};
    pub(crate) struct TaskMutexWaitPreparation;
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
    pub(crate) use shared_file::{SharedFileError, SharedFileId};
    mod address;
    pub(crate) mod page_table;
    mod permissions;
    mod retire;
    mod shared_file;
    #[path = "shared_memory/statistics.rs"]
    mod shared_memory;
    mod swappable_page;
    pub(crate) use address::PhysicalPageNumber;
    pub(crate) use config::PAGE_SIZE;
    pub(crate) use shared_file::SharedFrame;
    pub(crate) mod swap;
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
                crate::FRAME_FREED.with(|freed| freed.set(true));
                // SAFETY: 原地址与相同 layout 交还分配器。
                unsafe {
                    dealloc(
                        self.ppn.as_page_mut_ptr(),
                        Layout::from_size_align(4096, 4096).unwrap(),
                    )
                }
            }
        }
        pub(crate) fn alloc() -> Option<FrameTracker> {
            if crate::REQUIRE_RECLAIM.with(core::cell::Cell::get) {
                if !crate::FRAME_FREED.with(|freed| freed.replace(false)) {
                    return None;
                }
                crate::REQUIRE_RECLAIM.with(|required| required.set(false));
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
        use super::{
            SharedFileError, SharedFileId,
            address::{PhysicalPageNumber, VirtualAddress, VirtualPageNumber},
            frame_allocator::{FrameTracker, alloc},
            page_table::{PagePermissions, PageTable},
            permissions::MapPermission,
        };
        use crate::fallible_tree::FallibleMap;
        use shootdown::TranslationCommit;
        type TranslationSynchronizationError = ();
        use error::{MemoryError, UserAccessError};
        use user_access::UserFaultLimits;
        mod error;
        mod futex_atomic;
        mod private_fault;
        mod private_reclaim;
        mod resident;
        mod swap;
        mod user_access;
        use futex_atomic::UserAtomicU32;
        use resident::{PrivateBacking, PrivateResident};
        #[derive(Clone, Copy, PartialEq, Eq)]
        enum PageFaultAccess {
            Read,
            Write,
            Execute,
        }
        #[derive(Clone, Copy, PartialEq, Eq)]
        enum MapType {
            Framed,
        }
        struct MapArea {
            vpn_range: core::ops::Range<VirtualPageNumber>,
            map_permission: MapPermission,
            map_type: MapType,
            private_file: Option<()>,
            shared_anonymous: Option<()>,
            shared_file: Option<()>,
            device: Option<()>,
            data_frames: FallibleMap<VirtualPageNumber, PrivateResident>,
        }
        impl MapArea {
            fn has_leaf_permission(permission: MapPermission) -> bool {
                permission.intersects(MapPermission::R | MapPermission::W | MapPermission::X)
            }
        }
        struct MemorySet {
            page_table: PageTable,
            areas: FallibleMap<VirtualPageNumber, MapArea>,
            private_reclaim_cursor: VirtualPageNumber,
            private_reclaim_exclusion: Option<core::ops::Range<VirtualPageNumber>>,
        }
        pub(crate) mod shootdown {
            use crate::{EVENTS, REJECT_AFTER_REVOKE, REJECT_ALLOCATION};
            #[derive(Clone, Copy, Debug, PartialEq, Eq)]
            pub(crate) enum TranslationTransition {
                Publish,
                Relax,
                Revoke,
            }
            pub(crate) struct TranslationCommit {
                transition: Option<TranslationTransition>,
            }
            impl TranslationCommit {
                pub(crate) fn new() -> Self {
                    Self { transition: None }
                }
                pub(crate) fn stale_fault(page: usize) -> Self {
                    let mut commit = Self::new();
                    commit.record(page, TranslationTransition::Relax);
                    commit
                }
                pub(crate) fn record(&mut self, page: usize, transition: TranslationTransition) {
                    self.record_range(page, 1, transition);
                }
                pub(crate) fn record_range(
                    &mut self,
                    page: usize,
                    _: usize,
                    transition: TranslationTransition,
                ) {
                    self.transition = Some(transition);
                    EVENTS.with_borrow_mut(|events| events.push((page, transition)));
                }
                pub(crate) fn record_instruction_publication(&mut self, _: usize, _: usize) {}
                pub(crate) fn retain_table_pages<T>(&mut self, pages: T) {
                    drop(pages);
                }
                pub(crate) fn synchronize(&mut self) -> Result<(), ()> {
                    if self.transition == Some(TranslationTransition::Revoke)
                        && REJECT_AFTER_REVOKE.with(core::cell::Cell::get)
                    {
                        REJECT_ALLOCATION.with(|reject| reject.set(true));
                    }
                    Ok(())
                }
            }
        }
        enum PageFaultOutcome {
            Handled,
        }
        impl MemorySet {
            fn handle_page_fault_access(
                &mut self,
                address: usize,
                access: PageFaultAccess,
                limits: UserFaultLimits,
                force: bool,
            ) -> Result<PageFaultOutcome, MemoryError> {
                if self.handle_private_resident_fault(address, access, force)? {
                    return Ok(PageFaultOutcome::Handled);
                }
                self.handle_page_fault_with_limits(address, access, limits)
            }
            // 测试边界：替换 VMA/文件 fault-source 装配；分配、self reclaim、resident
            // fault、user-copy 与页表均执行生产代码，不在此复制回收/范围排除策略。
            fn handle_page_fault_with_limits(
                &mut self,
                address: usize,
                access: PageFaultAccess,
                _: UserFaultLimits,
            ) -> Result<PageFaultOutcome, MemoryError> {
                if self.handle_private_resident_fault(address, access, false)? {
                    return Ok(PageFaultOutcome::Handled);
                }
                let vpn = VirtualAddress::from(address).floor();
                let mut frame = self.allocate_private_frame()?;
                frame.bytes_mut().fill(0x6b);
                let resident = PrivateResident::user(frame)?;
                let (_, area) = self.areas.floor_mut(&vpn).unwrap();
                let flags = resident.permissions(area.map_permission, area.private_file.is_some());
                let entry = area
                    .data_frames
                    .try_prepare_vacant(vpn, resident)
                    .map_err(|_| MemoryError::OutOfMemory)?;
                let mut commit = TranslationCommit::new();
                self.page_table.map(
                    vpn,
                    entry.value().pin_resident().unwrap().unwrap().ppn(),
                    flags,
                    &mut commit,
                )?;
                area.data_frames.commit_vacant(entry);
                commit.synchronize().unwrap();
                Ok(PageFaultOutcome::Handled)
            }
        }
        fn memory(
            private_file: bool,
            discardable: bool,
            shared: bool,
            mapped: bool,
        ) -> (MemorySet, Option<PrivateResident>) {
            crate::EVENTS.with_borrow_mut(|events| {
                events.clear();
                events.reserve(16);
            });
            let vpn = VirtualPageNumber::from_vpn(0x4_0201);
            let mut raw = super::frame_allocator::alloc().unwrap();
            raw.bytes_mut().fill(0x5a);
            let mut resident = PrivateResident::user(raw).unwrap();
            let peer = shared.then(|| resident.clone());
            resident.discardable = discardable;
            let permission =
                MapPermission::U | MapPermission::R | MapPermission::W | MapPermission::X;
            let mut table = PageTable::try_new(crate::arch::mmu::AddressSpaceKind::User).unwrap();
            if mapped {
                table
                    .map(
                        vpn,
                        resident.pin_resident().unwrap().unwrap().ppn(),
                        resident.permissions(permission, private_file),
                        &mut TranslationCommit::new(),
                    )
                    .unwrap();
            }
            let mut area = MapArea {
                vpn_range: vpn..VirtualPageNumber::from_vpn(vpn.as_usize() + 1),
                map_permission: permission,
                map_type: MapType::Framed,
                private_file: private_file.then_some(()),
                shared_anonymous: None,
                shared_file: None,
                device: None,
                data_frames: FallibleMap::new(),
            };
            area.data_frames.try_insert(vpn, resident).unwrap();
            let mut areas = FallibleMap::new();
            areas.try_insert(vpn, area).unwrap();
            crate::EVENTS.with_borrow_mut(Vec::clear);
            (
                MemorySet {
                    page_table: table,
                    areas,
                    private_reclaim_cursor: vpn,
                    private_reclaim_exclusion: None,
                },
                peer,
            )
        }
        fn content(record: &PrivateResident) -> Vec<u8> {
            let mut bytes = vec![0; 4096];
            record
                .mapping()
                .unwrap()
                .page()
                .pin()
                .unwrap()
                .read(0, &mut bytes);
            bytes
        }
        impl MemorySet {
            fn restore_shared_swap_entries(
                &mut self,
                _: super::SharedFileId,
            ) -> Result<(), MemoryError> {
                Ok(())
            }
        }
        const ADDRESS: usize = 0x4_0201 * 4096;
        fn resident(memory: &MemorySet) -> &PrivateResident {
            memory
                .areas
                .first_key_value()
                .unwrap()
                .1
                .data_frames
                .first_key_value()
                .unwrap()
                .1
        }
        #[test]
        fn retained_read_and_execute_restore_the_existing_frame_and_write_tracking() {
            for access in [PageFaultAccess::Read, PageFaultAccess::Execute] {
                for (file, discardable, shared) in [
                    (true, false, false),
                    (false, true, false),
                    (false, false, true),
                    (false, false, false),
                ] {
                    let (mut memory, _peer) = memory(file, discardable, shared, false);
                    let old = resident(&memory).pin_resident().unwrap().unwrap().ppn();
                    assert!(
                        memory
                            .handle_private_resident_fault(ADDRESS, access, false)
                            .unwrap()
                    );
                    let leaf = memory
                        .page_table
                        .translate(VirtualAddress::from(ADDRESS).floor())
                        .unwrap();
                    assert_eq!(leaf.ppn(), old);
                    assert_eq!(
                        leaf.permissions().contains(PagePermissions::WRITE),
                        !(file || discardable || shared)
                    );
                    assert_eq!(content(resident(&memory)), &[0x5a; 4096]);
                    assert!(!resident(&memory).dirty);
                    assert_eq!(resident(&memory).discardable, discardable);
                }
            }
        }
        #[test]
        fn retained_write_restores_a_missing_leaf_and_commits_metadata() {
            for shared in [false, true] {
                let (mut memory, peer) = memory(true, true, shared, false);
                let old = resident(&memory).pin_resident().unwrap().unwrap().ppn();
                assert!(
                    memory
                        .handle_private_resident_fault(ADDRESS, PageFaultAccess::Write, false)
                        .unwrap()
                );
                assert_eq!(
                    resident(&memory).pin_resident().unwrap().unwrap().ppn() == old,
                    !shared
                );
                assert!(resident(&memory).dirty);
                assert!(!resident(&memory).discardable);
                assert_eq!(content(resident(&memory)), &[0x5a; 4096]);
                if let Some(peer) = peer {
                    assert_eq!(content(&peer), &[0x5a; 4096]);
                }
            }
        }
        #[test]
        fn failed_cow_preserves_old_leaf_and_reclaim_metadata() {
            let (mut memory, _peer) = memory(true, true, true, true);
            let old = resident(&memory).pin_resident().unwrap().unwrap().ppn();
            crate::REJECT_ALLOCATION.with(|reject| reject.set(true));
            let result =
                memory.handle_private_resident_fault(ADDRESS, PageFaultAccess::Write, false);
            crate::REJECT_ALLOCATION.with(|reject| reject.set(false));
            assert!(matches!(result, Err(MemoryError::OutOfMemory)));
            assert_eq!(
                memory
                    .page_table
                    .translate(VirtualAddress::from(ADDRESS).floor())
                    .unwrap()
                    .ppn(),
                old
            );
            assert!(!resident(&memory).dirty);
            assert!(resident(&memory).discardable);
            crate::EVENTS.with_borrow(|events| assert!(events.is_empty()));
        }
        #[test]
        fn cow_break_before_make_does_not_allocate_after_revoke_fence() {
            let (mut memory, peer) = memory(true, false, true, true);
            let old = resident(&memory).pin_resident().unwrap().unwrap().ppn();
            crate::REJECT_AFTER_REVOKE.with(|reject| reject.set(true));
            let result =
                memory.handle_private_resident_fault(ADDRESS, PageFaultAccess::Write, false);
            let allocation_was_rejected = super::frame_allocator::alloc().is_none();
            crate::REJECT_ALLOCATION.with(|reject| reject.set(false));
            crate::REJECT_AFTER_REVOKE.with(|reject| reject.set(false));
            assert!(result.unwrap());
            assert!(allocation_was_rejected);
            assert_ne!(
                resident(&memory).pin_resident().unwrap().unwrap().ppn(),
                old
            );
            assert_eq!(content(&peer.unwrap()), &[0x5a; 4096]);
            crate::EVENTS.with_borrow(|events| {
                assert_eq!(
                    events.as_slice(),
                    &[
                        (ADDRESS / 4096, shootdown::TranslationTransition::Revoke),
                        (ADDRESS / 4096, shootdown::TranslationTransition::Publish)
                    ]
                )
            });
        }
        fn two_page_copy(extra_reclaimable: bool, private_file: bool) {
            let (mut memory, _peer) = memory(private_file, !private_file, false, true);
            let vpn = VirtualAddress::from(ADDRESS).floor();
            let (_, area) = memory.areas.floor_mut(&vpn).unwrap();
            area.vpn_range.end = VirtualPageNumber::from_vpn(vpn.as_usize() + 3);
            if extra_reclaimable {
                let extra_vpn = VirtualPageNumber::from_vpn(vpn.as_usize() + 2);
                let mut extra =
                    PrivateResident::user(super::frame_allocator::alloc().unwrap()).unwrap();
                extra.discardable = true;
                memory
                    .page_table
                    .map(
                        extra_vpn,
                        extra.pin_resident().unwrap().unwrap().ppn(),
                        extra.permissions(area.map_permission, private_file),
                        &mut TranslationCommit::new(),
                    )
                    .unwrap();
                area.data_frames.try_insert(extra_vpn, extra).unwrap();
            }
            let mut destination = [0xcc; 8192];
            crate::FRAME_FREED.with(|freed| freed.set(false));
            crate::REQUIRE_RECLAIM.with(|required| required.set(true));
            let result = memory.copy_from_user(
                ADDRESS,
                &mut destination,
                UserFaultLimits::existing_mappings(),
            );
            crate::REQUIRE_RECLAIM.with(|required| required.set(false));
            assert!(memory.private_reclaim_exclusion.is_none());
            assert!(memory.page_table.translate(vpn).is_some());
            assert_eq!(content(resident(&memory)), &[0x5a; 4096]);
            if extra_reclaimable {
                assert_eq!(result, Ok(()));
                assert_eq!(&destination[..4096], &[0x5a; 4096]);
                assert_eq!(&destination[4096..], &[0x6b; 4096]);
                assert!(
                    !memory
                        .areas
                        .floor(&vpn)
                        .unwrap()
                        .1
                        .data_frames
                        .contains_key(&VirtualPageNumber::from_vpn(vpn.as_usize() + 2))
                );
            } else {
                assert_eq!(result, Err(UserAccessError::OutOfMemory));
                assert_eq!(destination, [0xcc; 8192]);
            }
            // 临时排除已恢复；原先保护的第一页现在仍可由同一 reclaimer 回收。
            assert_eq!(
                memory
                    .reclaim_private_pages(ReclaimRequest::for_target(1))
                    .reclaimed_pages(),
                1
            );
            assert!(memory.page_table.translate(vpn).is_none());
        }
        #[test]
        fn full_read_preparation_keeps_clean_private_file_pages_during_self_reclaim() {
            two_page_copy(true, true);
            two_page_copy(false, true);
        }
        #[test]
        fn full_read_preparation_keeps_madv_free_pages_during_self_reclaim() {
            two_page_copy(true, false);
            two_page_copy(false, false);
        }
    }
}
