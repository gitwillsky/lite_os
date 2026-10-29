use crate::memory::swap;
// Actual shared-anonymous backing, COW fork, MM revoke/unuse and statistics; CPU fences are host seams.
#[derive(Debug, Default)]
struct ShmVmaNotifications {
    opened: core::sync::atomic::AtomicUsize,
    closed: core::sync::atomic::AtomicUsize,
}
impl crate::memory::SharedMemoryVma for ShmVmaNotifications {
    fn open(&self) { self.opened.fetch_add(1, core::sync::atomic::Ordering::Relaxed); }
    fn close(&self) { self.closed.fetch_add(1, core::sync::atomic::Ordering::Relaxed); }
}
fn shm_vma_area(notifications: Arc<ShmVmaNotifications>) -> MapArea {
    let backing = shared_area::AnonymousSharedBacking::allocate(3).unwrap();
    let source = crate::memory::SharedMemoryMappingSource { backing: backing.clone(), lifetime: notifications, may_write: false };
    let mut area = MapArea::anonymous(0x50000000usize.into(), 0x50003000usize.into(), MapPermission::U | MapPermission::R);
    area.shared_anonymous = Some(SharedAnonymousArea {
        backing, page_offset: 0,
        sysv: Some(crate::memory::shared_memory::SharedMemoryVmaMembership::new(source)),
    });
    area
}
#[test]
fn actual_shared_anonymous_partition_notifies_only_nonempty_vma_members() {
    let _serial = SWAP_TEST.lock().unwrap();
    let notifications = Arc::new(ShmVmaNotifications::default());
    let area = shm_vma_area(notifications.clone());
    let start = area.vpn_range.start;
    let end = area.vpn_range.end;
    let (left, middle, right) = area.partition_protectable(start, end);
    assert!(left.is_none() && right.is_none());
    assert_eq!(notifications.opened.load(core::sync::atomic::Ordering::Relaxed), 1);
    assert_eq!(notifications.closed.load(core::sync::atomic::Ordering::Relaxed), 0);
    let (left, middle, right) = middle.partition_protectable(
        VirtualPageNumber::from_vpn(start.as_usize() + 1), VirtualPageNumber::from_vpn(start.as_usize() + 2));
    assert_eq!(notifications.opened.load(core::sync::atomic::Ordering::Relaxed), 3);
    assert_eq!(left.as_ref().unwrap().shared_anonymous.as_ref().unwrap().page_offset, 0);
    assert_eq!(middle.shared_anonymous.as_ref().unwrap().page_offset, 1);
    assert_eq!(right.as_ref().unwrap().shared_anonymous.as_ref().unwrap().page_offset, 2);
    assert!(!middle.shared_anonymous.as_ref().unwrap().sysv.as_ref().unwrap().may_write);
    drop(middle);
    assert_eq!(notifications.closed.load(core::sync::atomic::Ordering::Relaxed), 1);
    drop((left, right));
    assert_eq!(notifications.closed.load(core::sync::atomic::Ordering::Relaxed), 3);
}
#[test]
fn stack_partition_keeps_actual_grow_down_fragments_without_unique_stack_accounting() {
    let _serial = SWAP_TEST.lock().unwrap();
    let mut area = MapArea::stack(0x51008000);
    area.vpn_range.start = VirtualPageNumber::from_vpn(0x51000);
    let (left, middle, right) = area.partition_protectable(
        VirtualPageNumber::from_vpn(0x51003), VirtualPageNumber::from_vpn(0x51005));
    let left = left.unwrap();
    let right = right.unwrap();
    assert_eq!(left.kind, VmaKind::Stack);
    assert_eq!(middle.kind, VmaKind::Stack);
    assert_eq!(right.kind, VmaKind::Stack);
    let mut memory = MemorySet::empty();
    for fragment in [left, middle, right] {
        memory.commit_area(FallibleMap::try_prepare(fragment.vpn_range.start, fragment).unwrap());
    }
    assert_eq!(memory.vma_index_state.virtual_bytes(), 8 * 4096);
    assert_eq!(memory.vma_index_state.data_bytes(), 0);
    let removed = memory.remove_area(&VirtualPageNumber::from_vpn(0x51003)).unwrap();
    assert_eq!(memory.vma_index_state.virtual_bytes(), 6 * 4096);
    assert_eq!(removed.kind, VmaKind::Stack);
    drop((removed, memory));
}
#[test]
fn actual_fork_adds_one_vma_member_and_backing_observers_add_none() {
    let _serial = SWAP_TEST.lock().unwrap();
    let notifications = Arc::new(ShmVmaNotifications::default());
    let area = shm_vma_area(notifications.clone());
    let backing_observer = area.shared_anonymous.as_ref().unwrap().backing.clone();
    let mut parent = MemorySet::empty();
    parent.commit_area(FallibleMap::try_prepare(area.vpn_range.start, area).unwrap());
    let mut child = MemorySet::empty();
    parent.try_clone_for_fork(&mut child).unwrap();
    assert_eq!(notifications.opened.load(core::sync::atomic::Ordering::Relaxed), 2);
    assert_eq!(notifications.closed.load(core::sync::atomic::Ordering::Relaxed), 0);
    drop(child);
    assert_eq!(notifications.closed.load(core::sync::atomic::Ordering::Relaxed), 1);
    drop(parent);
    assert_eq!(notifications.closed.load(core::sync::atomic::Ordering::Relaxed), 2);
    drop(backing_observer);
    assert_eq!(notifications.closed.load(core::sync::atomic::Ordering::Relaxed), 2);
}
#[test]
fn shm_unevictable_policy_covers_direct_page_and_backing_reclaim_and_does_not_fault_in() {
    let _serial = SWAP_TEST.lock().unwrap();
    let disk = SwapDisk::new(9);
    swap::activate(disk.clone(), b"/dev/test", 0).unwrap();
    let backing = shared_area::AnonymousSharedBacking::allocate(1).unwrap();
    let page = backing.page(0).unwrap();
    page.pin().unwrap().write(0, &[0x88; 4096]);
    let policy = backing.freeze_swap_policy().unwrap();
    assert!(!page.swap_out(), "policy transaction must fence direct swap admission");
    policy.set_locked(true);
    drop(policy);
    assert!(!page.swap_out());
    assert_eq!(backing.reclaim_pages(background_request()).reclaimed_pages(), 0);
    assert_eq!(backing.shared_memory_statistics(), (1, 0));
    let policy = backing.freeze_swap_policy().unwrap();
    policy.set_locked(false);
    drop(policy);
    assert!(page.swap_out());
    assert_eq!(backing.shared_memory_statistics(), (0, 1));
    let policy = backing.freeze_swap_policy().unwrap();
    policy.set_locked(true);
    drop(policy);
    assert_eq!(backing.shared_memory_statistics(), (0, 1), "SHM_LOCK is not an eager fault-in");
    let mut bytes = [0; 4096];
    page.pin().unwrap().read(0, &mut bytes);
    assert_eq!(bytes, [0x88; 4096]);
    assert!(!page.swap_out());
    drop((page, backing));
    let mut wait = crate::execution::sync::TaskMutexWaitPreparation;
    swap::deactivate(SharedFileId { filesystem: 999, inode: 1 }, &mut wait).unwrap();
}
#[test]
fn shm_lock_waits_for_actual_inflight_swap_io_then_prevents_later_swap() {
    use std::{sync::mpsc, time::Duration};
    let _serial = SWAP_TEST.lock().unwrap();
    let disk = SwapDisk::new(9);
    swap::activate(disk.clone(), b"/dev/test", 0).unwrap();
    let backing = shared_area::AnonymousSharedBacking::allocate(1).unwrap();
    let page = backing.page(0).unwrap();
    page.pin().unwrap().write(0, &[0x31; 4096]);
    let (entered, entering) = mpsc::channel();
    let (resume, resumed) = mpsc::channel();
    *disk.write_hook.lock().unwrap() = Some(Box::new(move || {
        entered.send(()).unwrap();
        resumed.recv().unwrap();
    }));
    let migrating = page.clone();
    let migrator = std::thread::spawn(move || migrating.swap_out());
    entering.recv_timeout(Duration::from_secs(2)).unwrap();
    let (started, starting) = mpsc::channel();
    let (locked, locking) = mpsc::channel();
    let locking_backing = backing.clone();
    let locker = std::thread::spawn(move || {
        started.send(()).unwrap();
        let policy = locking_backing.freeze_swap_policy().unwrap();
        policy.set_locked(true);
        drop(policy);
        locked.send(()).unwrap();
    });
    starting.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(locking.try_recv().is_err(), "locked publication cannot precede inflight IO completion");
    resume.send(()).unwrap();
    assert!(migrator.join().unwrap());
    locking.recv_timeout(Duration::from_secs(2)).unwrap();
    locker.join().unwrap();
    assert!(backing.locked());
    assert_eq!(backing.shared_memory_statistics(), (0, 1));
    assert!(!page.swap_out());
    let mut bytes = [0; 4096];
    page.pin().unwrap().read(0, &mut bytes);
    assert_eq!(bytes, [0x31; 4096]);
    assert!(!page.swap_out());
    drop((page, backing));
    let mut wait = crate::execution::sync::TaskMutexWaitPreparation;
    swap::deactivate(SharedFileId { filesystem: 999, inode: 1 }, &mut wait).unwrap();
}
#[test]
fn shared_anonymous_aliases_keep_shared_identity_and_are_excluded_from_vmswap() {
    use crate::memory::swap;
    let _serial = SWAP_TEST.lock().unwrap();
    for no_access in [false, true] {
        let request = background_request();
        let disk = SwapDisk::new(9);
        swap::activate(disk.clone(), b"/dev/test", 0).unwrap();
        let backing = shared_area::AnonymousSharedBacking::allocate(2).unwrap();
        let page = backing.page(0).unwrap();
        let vpn = VirtualPageNumber::from_vpn(0x5_0401);
        let mut memory = MemorySet::empty();
        let permission = if no_access {
            MapPermission::U
        } else {
            MapPermission::U | MapPermission::R | MapPermission::W
        };
        let mut area = MapArea::anonymous(
            vpn.into(),
            VirtualPageNumber::from_vpn(vpn.as_usize() + 1).into(),
            permission,
        );
        area.shared_anonymous = Some(SharedAnonymousArea {
            backing: backing.clone(),
            page_offset: 0,
            sysv: None,
        });
        {
            let pin = page.pin().unwrap();
            pin.write(0, &[0x66; 4096]);
            area.data_frames
                .try_insert(vpn, PrivateResident::shared(&page, &pin, false))
                .unwrap();
            if !no_access {
                memory
                    .page_table
                    .map(
                        vpn,
                        pin.ppn(),
                        permission.into(),
                        &mut TranslationCommit::new(),
                    )
                    .unwrap();
            }
        }
        memory.commit_area(FallibleMap::try_prepare(vpn, area).unwrap());
        let parent = Arc::new(SwapMemory::new(memory));
        let child = Arc::new(SwapMemory::new(MemorySet::empty()));
        shared_file::register_memory_mapping_owner(parent.clone()).unwrap();
        shared_file::register_memory_mapping_owner(child.clone()).unwrap();
        {
            let mut source = parent.memory.lock().unwrap();
            source
                .try_clone_for_fork(&mut child.memory.lock().unwrap())
                .unwrap();
            shared_file::complete_memory_mapping_owner_initialization();
        }
        assert_eq!(backing.reclaim_pages(request).reclaimed_pages(), 1);
        for owner in [&parent, &child] {
            let memory = owner.memory.lock().unwrap();
            let pages = memory.user_page_statistics();
            assert_eq!(
                (
                    pages.resident_pages,
                    pages.shared_pages,
                    pages.swapped_pages
                ),
                (0, 0, 0)
            );
            assert!(memory.page_table.translate(vpn).is_none());
        }
        {
            parent
                .memory
                .lock()
                .unwrap()
                .restore_swapped_pages(SharedFileId {
                    filesystem: 999,
                    inode: 1,
                })
                .unwrap();
            let pages = child.memory.lock().unwrap().user_page_statistics();
            assert_eq!((pages.resident_pages, pages.swapped_pages), (0, 0));
            page.pin().unwrap().write(4095, &[0xab]);
        }
        assert!(
            page.swap_out(),
            "resident alias and stale sibling must share one new slot epoch"
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
        for owner in [&parent, &child] {
            let memory = owner.memory.lock().unwrap();
            let pages = memory.user_page_statistics();
            assert_eq!(
                (
                    pages.resident_pages,
                    pages.shared_pages,
                    pages.swapped_pages
                ),
                (1, 1, 0)
            );
            let leaf = memory.page_table.translate(vpn);
            assert_eq!(leaf.is_some(), !no_access);
            if let Some(leaf) = leaf {
                assert!(leaf.permissions().contains(PagePermissions::WRITE));
                assert_eq!(leaf.ppn(), page.pin().unwrap().ppn());
            }
        }
        let mut bytes = [0; 4096];
        page.pin().unwrap().read(0, &mut bytes);
        assert_eq!(&bytes[..4095], &[0x66; 4095]);
        assert_eq!(bytes[4095], 0xab);
    }
}

#[test]
fn shared_page_aggregate_follows_actual_publication_failure_and_physical_migration() {
    use core::sync::atomic::Ordering;
    let _serial = SWAP_TEST.lock().unwrap();
    let disk = SwapDisk::new(9);
    swap::activate(disk.clone(), b"/dev/test", 0).unwrap();
    let backing = shared_area::AnonymousSharedBacking::allocate(3).unwrap();
    let statistics = shared_area::statistics_observer(&backing);
    assert_eq!(backing.shared_memory_statistics(), (0, 0));
    crate::memory::frame_allocator::FAIL_NEXT_FRAME.with(|flag| flag.set(true));
    assert!(backing.page(0).is_err());
    assert_eq!(statistics.snapshot(), (0, 0), "failed preparation has no membership");
    let first = backing.page(0).unwrap();
    let first_observer = backing.page(0).unwrap();
    assert!(Arc::ptr_eq(&first, &first_observer));
    assert_eq!(statistics.snapshot(), (1, 0), "same indexed page is admitted once");
    let second = backing.page(1).unwrap();
    assert_eq!(statistics.snapshot(), (2, 0));
    let during_write = statistics.clone();
    *disk.write_hook.lock().unwrap() = Some(Box::new(move || {
        assert_eq!(during_write.snapshot(), (2, 0), "IO preparation does not publish swap residency");
    }));
    disk.write_error.store(true, Ordering::Relaxed);
    assert!(!first.swap_out());
    assert_eq!(statistics.snapshot(), (2, 0));
    disk.write_error.store(false, Ordering::Relaxed);
    assert!(first.swap_out());
    assert_eq!(statistics.snapshot(), (1, 1));
    disk.read_error.store(true, Ordering::Relaxed);
    assert!(first.pin().is_err());
    assert_eq!(statistics.snapshot(), (1, 1));
    disk.read_error.store(false, Ordering::Relaxed);
    let during_read = statistics.clone();
    *disk.read_hook.lock().unwrap() = Some(Box::new(move || {
        assert_eq!(during_read.snapshot(), (1, 1), "IO preparation does not publish resident content");
    }));
    drop(first.pin().unwrap());
    assert_eq!(statistics.snapshot(), (2, 0));
    assert!(second.swap_out());
    assert_eq!(statistics.snapshot(), (1, 1));
    drop((first, second));
    assert_eq!(statistics.snapshot(), (1, 1), "backing retains unmapped physical content");
    drop(backing);
    assert_eq!(statistics.snapshot(), (1, 0), "page observer owns only its remaining content");
    drop(first_observer);
    assert_eq!(statistics.snapshot(), (0, 0), "actual last page retirement removes membership");
    let mut wait = crate::execution::sync::TaskMutexWaitPreparation;
    swap::deactivate(SharedFileId { filesystem: 999, inode: 1 }, &mut wait).unwrap();
}

#[test]
fn concurrent_shared_faults_admit_only_one_actual_indexed_page() {
    let _serial = SWAP_TEST.lock().unwrap();
    let backing = shared_area::AnonymousSharedBacking::allocate(1).unwrap();
    let statistics = shared_area::statistics_observer(&backing);
    let start = Arc::new(std::sync::Barrier::new(8));
    let workers: Vec<_> = (0..8).map(|_| {
        let backing = backing.clone();
        let start = start.clone();
        std::thread::spawn(move || { start.wait(); backing.page(0).unwrap() })
    }).collect();
    let pages: Vec<_> = workers.into_iter().map(|worker| worker.join().unwrap()).collect();
    assert!(pages.iter().all(|page| Arc::ptr_eq(page, &pages[0])));
    assert_eq!(statistics.snapshot(), (1, 0));
    drop(backing);
    assert_eq!(statistics.snapshot(), (1, 0));
    drop(pages);
    assert_eq!(statistics.snapshot(), (0, 0));
}
