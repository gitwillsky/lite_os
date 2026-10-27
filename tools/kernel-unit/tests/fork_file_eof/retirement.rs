// Actual canonical Page, weak MM registry, production COW/revoke and reclaim adapter.
// Host fence is an explicit no-op; the count/version proof is tested, not hardware TLB behavior.

#[test]
fn repeated_registration_cannot_balance_a_missing_weak_dead_mm_reference() {
    use crate::memory::swap;
    use core::sync::atomic::Ordering;
    let _serial = SWAP_TEST.lock().unwrap();
    let disk = SwapDisk::new(9);
    swap::activate(disk.clone(), b"/dev/test", 0).unwrap();
    let parent = Arc::new(SwapMemory::new(swap_memory(false, 1)));
    let child = Arc::new(SwapMemory::new(MemorySet::empty()));
    shared_file::register_memory_mapping_owner(parent.clone()).unwrap();
    shared_file::register_memory_mapping_owner(parent.clone()).unwrap();
    shared_file::register_memory_mapping_owner(child.clone()).unwrap();
    shared_file::register_memory_reclaimer(parent.clone()).unwrap();
    {
        let mut parent_mm = parent.memory.lock().unwrap();
        let mut child_mm = child.memory.lock().unwrap();
        parent_mm.try_clone_for_fork(&mut child_mm).unwrap();
        shared_file::complete_memory_mapping_owner_initialization();
    }
    let vpn = VirtualPageNumber::from_vpn(0x5_0401);
    let (retiring, retired) = std::sync::mpsc::channel();
    let (resume, resumed) = std::sync::mpsc::channel();
    *child.before_drop.lock().unwrap() = Some(Box::new(move |memory| {
        // Arc strong count is zero before its fields retire; an actual retained leaf exists.
        assert!(memory.page_table.translate(vpn).is_some());
        retiring.send(()).unwrap();
        resumed.recv().unwrap();
    }));
    let weak = Arc::downgrade(&child);
    let dropper = std::thread::spawn(move || drop(child));
    retired.recv().unwrap();
    assert!(weak.upgrade().is_none());
    assert_eq!(
        shared_file::reclaim_background_pages(1).reclaimed_pages(),
        0
    );
    assert_eq!(swap::statistics(), (7, 7));
    {
        let memory = parent.memory.lock().unwrap();
        let record = memory
            .areas
            .first_key_value()
            .unwrap()
            .1
            .data_frames
            .get(&vpn)
            .unwrap();
        assert!(record.is_installed());
        assert!(!record.exclusive());
        assert_eq!(record.content_snapshot().unwrap().bytes(), &[0x51; 4096]);
    }
    resume.send(()).unwrap();
    dropper.join().unwrap();
    assert_eq!(
        shared_file::reclaim_background_pages(1).reclaimed_pages(),
        1
    );
    assert_eq!(swap::statistics(), (7, 6));
    assert!(
        !parent
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
            .is_installed()
    );
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
    assert!(
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
            .is_installed()
    );
}

#[test]
fn one_alias_fault_accounts_its_own_rss_and_discarding_stale_sibling_unpins_swapoff() {
    use crate::memory::swap;
    use core::sync::atomic::Ordering;
    let _serial = SWAP_TEST.lock().unwrap();
    let disk = SwapDisk::new(9);
    swap::activate(disk.clone(), b"/dev/test", 0).unwrap();
    let parent = Arc::new(SwapMemory::new(swap_memory(false, 1)));
    let child = Arc::new(SwapMemory::new(MemorySet::empty()));
    shared_file::register_memory_mapping_owner(parent.clone()).unwrap();
    shared_file::register_memory_mapping_owner(child.clone()).unwrap();
    shared_file::register_memory_reclaimer(parent.clone()).unwrap();
    {
        let mut parent_mm = parent.memory.lock().unwrap();
        let mut child_mm = child.memory.lock().unwrap();
        parent_mm.try_clone_for_fork(&mut child_mm).unwrap();
        shared_file::complete_memory_mapping_owner_initialization();
    }
    assert_eq!(
        shared_file::reclaim_background_pages(1).reclaimed_pages(),
        1
    );
    assert_eq!(swap::statistics(), (7, 6));
    for owner in [&parent, &child] {
        let pages = owner.memory.lock().unwrap().user_page_statistics();
        assert_eq!((pages.resident_pages, pages.swapped_pages), (0, 1));
    }
    let vpn = VirtualPageNumber::from_vpn(0x5_0401);
    {
        let mut parent_mm = parent.memory.lock().unwrap();
        assert!(
            parent_mm
                .handle_private_resident_fault(
                    vpn.as_usize() * config::PAGE_SIZE,
                    PageFaultAccess::Read, false
                )
                .unwrap()
        );
        let pages = parent_mm.user_page_statistics();
        assert_eq!((pages.resident_pages, pages.swapped_pages), (1, 0));
        assert!(
            !parent_mm
                .page_table
                .translate(vpn)
                .unwrap()
                .permissions()
                .contains(PagePermissions::WRITE)
        );
    }
    {
        let mut child_mm = child.memory.lock().unwrap();
        let pages = child_mm.user_page_statistics();
        assert_eq!((pages.resident_pages, pages.swapped_pages), (0, 1));
        child_mm
            .areas
            .floor_mut(&vpn)
            .unwrap()
            .1
            .data_frames
            .get_mut(&vpn)
            .unwrap()
            .discardable = true;
        let result = child_mm.reclaim_private_pages(ReclaimRequest::for_target(1));
        assert_eq!(
            result.reclaimed_pages(),
            0,
            "discarding one alias does not free sibling's frame"
        );
        assert_eq!(result.scanned_pages(), 1);
    }
    assert_eq!(swap::statistics(), (7, 7));
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
    let memory = parent.memory.lock().unwrap();
    let record = memory
        .areas
        .floor(&vpn)
        .unwrap()
        .1
        .data_frames
        .get(&vpn)
        .unwrap();
    assert!(record.exclusive());
    assert_eq!(record.content_snapshot().unwrap().bytes(), &[0x51; 4096]);
}
