// Force a fault in an already revoked MM while a later alias's MM stays busy.
#[test]
fn early_revoked_alias_waiting_for_page_operation_does_not_deadlock_later_busy_mm() {
    use crate::memory::swap;
    use std::{sync::mpsc, time::Duration};
    let _serial = SWAP_TEST.lock().unwrap();
    let disk = SwapDisk::new(9);
    swap::activate(disk, b"/dev/test", 0).unwrap();
    let parent = Arc::new(SwapMemory::new(swap_memory(false, 1)));
    let child = Arc::new(SwapMemory::new(MemorySet::empty()));
    shared_file::register_memory_mapping_owner(parent.clone()).unwrap();
    shared_file::register_memory_mapping_owner(child.clone()).unwrap();
    let vpn = VirtualPageNumber::from_vpn(0x5_0401);
    {
        let mut source = parent.memory.lock().unwrap();
        source
            .try_clone_for_fork(&mut child.memory.lock().unwrap())
            .unwrap();
        shared_file::complete_memory_mapping_owner_initialization();
    }
    let page = parent
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
        .mapping()
        .unwrap()
        .page()
        .clone();
    let (revoked, revocation) = mpsc::channel();
    *parent.after_revoke.lock().unwrap() = Some(Box::new(move || revoked.send(()).unwrap()));
    let (late_seen, late_visit) = mpsc::channel();
    let (continue_late, continued) = mpsc::channel();
    *child.before_revoke.lock().unwrap() = Some(Box::new(move || {
        late_seen.send(()).unwrap();
        continued.recv().unwrap();
    }));
    let child_guard = child.memory.lock().unwrap();
    let (migrated, migration) = mpsc::channel();
    let migrator = std::thread::spawn(move || migrated.send(page.swap_out()).unwrap());
    revocation.recv_timeout(Duration::from_secs(2)).unwrap();
    late_visit.recv_timeout(Duration::from_secs(2)).unwrap();
    let (acquired, acquisition) = mpsc::channel();
    let (restored, restoration) = mpsc::channel();
    let fault_owner = parent.clone();
    let fault = std::thread::spawn(move || {
        let mut memory = fault_owner.memory.lock().unwrap();
        assert!(memory.page_table.translate(vpn).is_none());
        acquired.send(()).unwrap();
        restored
            .send(
                memory
                    .handle_private_resident_fault(
                        vpn.as_usize() * config::PAGE_SIZE,
                        PageFaultAccess::Read, false,
                    )
                    .unwrap(),
            )
            .unwrap();
    });
    acquisition.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(
        restoration.try_recv().is_err(),
        "fault must wait for the physical page operation"
    );
    continue_late.send(()).unwrap();
    assert!(!migration.recv_timeout(Duration::from_secs(2)).unwrap());
    assert!(restoration.recv_timeout(Duration::from_secs(2)).unwrap());
    migrator.join().unwrap();
    fault.join().unwrap();
    drop(child_guard);
    assert_eq!(swap::statistics(), (7, 7));
    for owner in [&parent, &child] {
        let memory = owner.memory.lock().unwrap();
        let pages = memory.user_page_statistics();
        assert_eq!((pages.resident_pages, pages.swapped_pages), (1, 0));
        assert_eq!(
            memory
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
    }
    let mut wait = crate::execution::sync::TaskMutexWaitPreparation;
    swap::deactivate(
        SharedFileId {
            filesystem: 999,
            inode: 1,
        },
        &mut wait,
    )
    .unwrap();
}

#[test]
fn swapoff_waits_for_pre_draining_reservation_then_restores_its_completed_write() {
    use crate::memory::swap;
    use core::sync::atomic::Ordering;
    use std::{
        sync::mpsc,
        time::{Duration, Instant},
    };
    let _serial = SWAP_TEST.lock().unwrap();
    let disk = SwapDisk::new(9);
    swap::activate(disk.clone(), b"/dev/test", 0).unwrap();
    let page = crate::memory::SwappablePage::allocate_shared().unwrap();
    page.pin().unwrap().write(0, &[0x93; 4096]);
    let (reserved, reservation) = mpsc::channel();
    let (resume, resumed) = mpsc::channel();
    *disk.write_hook.lock().unwrap() = Some(Box::new(move || {
        reserved.send(()).unwrap();
        resumed.recv().unwrap();
    }));
    let migrating = page.clone();
    let migrator = std::thread::spawn(move || migrating.swap_out());
    reservation.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(swap::statistics(), (7, 6));
    let (drained, completion) = mpsc::channel();
    let drainer = std::thread::spawn(move || {
        let mut wait = crate::execution::sync::TaskMutexWaitPreparation;
        drained
            .send(swap::deactivate(
                SharedFileId {
                    filesystem: 999,
                    inode: 1,
                },
                &mut wait,
            ))
            .unwrap();
    });
    let deadline = Instant::now() + Duration::from_secs(2);
    while swap::reserve().is_some() {
        assert!(
            Instant::now() < deadline,
            "swapoff did not publish Draining"
        );
        std::thread::yield_now();
    }
    assert!(completion.try_recv().is_err());
    assert!(!disk.closed.load(Ordering::Acquire));
    resume.send(()).unwrap();
    assert!(migrator.join().unwrap());
    assert_eq!(
        completion.recv_timeout(Duration::from_secs(2)).unwrap(),
        Ok(())
    );
    drainer.join().unwrap();
    assert!(disk.closed.load(Ordering::Acquire));
    let mut bytes = [0; 4096];
    page.pin().unwrap().read(0, &mut bytes);
    assert_eq!(bytes, [0x93; 4096]);
}
