// Actual area, Page/MM restore and event cancellation; scheduler/signals are a host seam.
#[test]
fn interrupted_physical_drain_preserves_partial_restore_slots_and_storage_for_retry() {
    use crate::{interruptible_event_target::Target, memory::swap};
    use core::sync::atomic::Ordering;
    use swap::SwapStorage;
    let _serial = SWAP_TEST.lock().unwrap();
    let target = Target::new();
    let _binding = target.bind();
    let disk = SwapDisk::new(9);
    swap::activate(disk.clone(), b"/dev/test", 0).unwrap();
    let owner = Arc::new(SwapMemory::new(swap_memory(false, 2)));
    shared_file::register_memory_mapping_owner(owner.clone()).unwrap();
    shared_file::register_memory_reclaimer(owner.clone()).unwrap();
    assert_eq!(
        shared_file::reclaim_background_pages(2).reclaimed_pages(),
        2
    );
    assert_eq!(swap::statistics(), (7, 5));
    target.signal();
    let mut wait = crate::execution::sync::TaskMutexWaitPreparation;
    assert_eq!(
        swap::deactivate(disk.identity(), &mut wait),
        Err(swap::SwapError::Interrupted)
    );
    assert_eq!(swap::statistics(), (7, 5));
    assert!(!disk.closed.load(Ordering::Acquire));
    target.clear_signal();
    let signal_target = target.clone();
    *disk.read_hook.lock().unwrap() = Some(Box::new(move || signal_target.signal()));
    assert_eq!(
        swap::deactivate(disk.identity(), &mut wait),
        Err(swap::SwapError::Interrupted)
    );
    assert_eq!(swap::statistics(), (7, 5));
    assert!(!disk.closed.load(Ordering::Acquire));
    // Page-in completed for one page; neither logical alias was published by the canceled walk.
    let stats = owner.memory.lock().unwrap().user_page_statistics();
    assert_eq!(stats.resident_pages, 0);
    target.clear_signal();
    swap::deactivate(disk.identity(), &mut wait).unwrap();
    assert!(disk.closed.load(Ordering::Acquire));
    let mm = owner.memory.lock().unwrap();
    assert_eq!(mm.user_page_statistics().resident_pages, 2);
    for (index, page) in mm
        .areas
        .first_key_value()
        .unwrap()
        .1
        .data_frames
        .values()
        .enumerate()
    {
        assert!(
            page.content_snapshot()
                .unwrap()
                .bytes()
                .iter()
                .all(|byte| *byte == 0x51 + index as u8)
        );
    }
}

#[test]
fn signal_interrupts_final_slot_retirement_and_reactivation_keeps_the_lease() {
    use crate::{interruptible_event_target::Target, memory::swap};
    use core::sync::atomic::Ordering;
    use swap::SwapStorage;
    let _serial = SWAP_TEST.lock().unwrap();
    let disk = SwapDisk::new(9);
    swap::activate(disk.clone(), b"/dev/test", 0).unwrap();
    let orphan = swap::reserve().unwrap();
    let target = Target::new();
    let worker_target = target.clone();
    let identity = disk.identity();
    let drain = std::thread::spawn(move || {
        let _binding = worker_target.bind();
        swap::deactivate(identity, &mut crate::execution::sync::TaskMutexWaitPreparation)
    });
    target.wait_until_sleeping();
    assert!(swap::reserve().is_none());
    target.signal();
    assert_eq!(drain.join().unwrap(), Err(swap::SwapError::Interrupted));
    assert!(!disk.closed.load(Ordering::Acquire));
    assert_eq!(swap::statistics(), (7, 6));
    assert!(
        swap::reserve().is_some(),
        "Interrupted must restore Active admission"
    );
    drop(orphan);
    swap::deactivate(identity, &mut crate::execution::sync::TaskMutexWaitPreparation).unwrap();
    assert!(disk.closed.load(Ordering::Acquire));
    assert_eq!(swap::statistics(), (0, 0));
}

#[test]
fn empty_area_and_last_slot_restored_before_checkpoint_win_over_pending_signal() {
    use crate::{interruptible_event_target::Target, memory::swap};
    use core::sync::atomic::Ordering;
    use swap::SwapStorage;
    let _serial = SWAP_TEST.lock().unwrap();
    let target = Target::new();
    let _binding = target.bind();
    let disk = SwapDisk::new(9);
    swap::activate(disk.clone(), b"/dev/test", 0).unwrap();
    target.signal();
    let mut wait = crate::execution::sync::TaskMutexWaitPreparation;
    swap::deactivate(disk.identity(), &mut wait).unwrap();
    assert!(disk.closed.load(Ordering::Acquire));
    target.clear_signal();
    let disk = SwapDisk::new(9);
    swap::activate(disk.clone(), b"/dev/test", 0).unwrap();
    let owner = Arc::new(SwapMemory::new(swap_memory(false, 1)));
    let unrelated = Arc::new(SwapMemory::new(MemorySet::empty()));
    shared_file::register_memory_mapping_owner(owner.clone()).unwrap();
    shared_file::register_memory_mapping_owner(unrelated.clone()).unwrap();
    shared_file::register_memory_reclaimer(owner.clone()).unwrap();
    assert_eq!(
        shared_file::reclaim_background_pages(1).reclaimed_pages(),
        1
    );
    let signal_target = target.clone();
    *owner.before_restore.lock().unwrap() = Some(Box::new(move || signal_target.signal()));
    // The physical page is restored, then this MM installs the final logical alias.
    // The next unrelated MM checkpoint sees the signal after usage is already zero.
    swap::deactivate(disk.identity(), &mut wait).unwrap();
    assert!(disk.closed.load(Ordering::Acquire));
    assert_eq!(swap::statistics(), (0, 0));
    assert_eq!(
        owner
            .memory
            .lock()
            .unwrap()
            .user_page_statistics()
            .resident_pages,
        1
    );
}
