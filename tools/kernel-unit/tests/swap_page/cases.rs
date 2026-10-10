fn environment() -> Arc<storage::Storage> {
    let storage = storage::configure();
    REVOKE.with(|hook| *hook.borrow_mut() = Some(Box::new(|_, _| Some(0))));
    storage
}

fn mapped(private: bool) -> (Arc<SwappablePage>, PageMappingRef) {
    let page = SwappablePage::new(memory::SharedFrame::allocate().unwrap(), private).unwrap();
    let pin = page.pin().unwrap();
    pin.write(0, b"original");
    let reference = page.mapping(&pin, private);
    drop(pin);
    (page, reference)
}

fn revoke_count(count: usize) {
    REVOKE.with(|hook| *hook.borrow_mut() = Some(Box::new(move |_, _| Some(count))));
}

#[test]
fn shared_swapcache_has_one_slot_until_last_logical_swap_entry_is_installed() {
    let storage = environment();
    let (page, mut first) = mapped(true);
    let mut second = first.clone();
    assert!(!first.private_exclusive());
    revoke_count(2);
    assert!(page.swap_out());
    assert!(!first.is_installed() && !second.is_installed());
    assert_eq!(storage.reserved.load(Ordering::Relaxed), 1);
    let pin = page.pin().unwrap();
    let mut bytes = [0; 8];
    pin.read(0, &mut bytes);
    assert_eq!(&bytes, b"original");
    first.mark_installed(&pin);
    assert!(first.is_installed() && !second.is_installed());
    assert_eq!(storage.live_slots.load(Ordering::Relaxed), 1);
    // 最新 Resident 内容可以不同于保留的 swapcache bytes；第二 alias 必须读最新物理页。
    pin.write(0, b"modified");
    drop(pin);
    let pin = page.pin().unwrap();
    pin.read(0, &mut bytes);
    assert_eq!(&bytes, b"modified");
    second.mark_installed(&pin);
    assert_eq!(storage.live_slots.load(Ordering::Relaxed), 0);
    drop(pin);
    drop(second);
    assert!(first.private_exclusive());
}

#[test]
fn busy_mm_or_missing_retirement_reference_cancels_without_changing_logical_residency() {
    let storage = environment();
    let (page, first) = mapped(false);
    let second = first.clone();
    REVOKE.with(|hook| *hook.borrow_mut() = Some(Box::new(|_, _| None)));
    assert!(!page.swap_out());
    assert!(first.is_installed() && second.is_installed());
    revoke_count(1);
    assert!(!page.swap_out());
    assert!(first.is_installed() && second.is_installed());
    assert_eq!(storage.live_slots.load(Ordering::Relaxed), 0);
    drop(second);
    assert!(page.swap_out());
    assert!(!first.is_installed());
}

#[test]
fn write_io_error_preserves_physical_content_and_old_epoch_after_all_revokes() {
    let storage = environment();
    let (page, reference) = mapped(true);
    revoke_count(1);
    storage.fail_write.store(true, Ordering::Relaxed);
    assert!(!page.swap_out());
    assert!(reference.is_installed());
    assert_eq!(storage.live_slots.load(Ordering::Relaxed), 0);
    let mut bytes = [0; 8];
    page.pin().unwrap().read(0, &mut bytes);
    assert_eq!(&bytes, b"original");
}

#[test]
fn reference_drop_during_io_cancels_post_io_publication() {
    let storage = environment();
    let (page, reference) = mapped(true);
    let sibling = reference.clone();
    revoke_count(2);
    WRITE_HOOK.with(|hook| *hook.borrow_mut() = Some(Box::new(move || drop(sibling))));
    assert!(!page.swap_out());
    assert!(reference.is_installed() && reference.private_exclusive());
    assert_eq!(storage.live_slots.load(Ordering::Relaxed), 0);
}

#[test]
fn page_in_frame_oom_and_read_io_error_leave_the_original_slot_and_can_retry() {
    let storage = environment();
    let (page, mut reference) = mapped(true);
    revoke_count(1);
    assert!(page.swap_out());
    FRAME_OOM.with(|value| value.set(true));
    assert!(matches!(
        page.pin(),
        Err(memory::SharedFileError::OutOfMemory)
    ));
    storage.fail_read.store(true, Ordering::Relaxed);
    assert!(matches!(page.pin(), Err(memory::SharedFileError::Io)));
    assert_eq!(storage.live_slots.load(Ordering::Relaxed), 1);
    assert!(!reference.is_installed());
    storage.fail_read.store(false, Ordering::Relaxed);
    let pin = page.pin().unwrap();
    reference.mark_installed(&pin);
    assert_eq!(storage.live_slots.load(Ordering::Relaxed), 0);
}

#[test]
fn pinned_kernel_byte_access_cannot_be_migrated_and_resident_probe_does_not_page_in() {
    environment();
    let (page, reference) = mapped(false);
    revoke_count(1);
    let pin = page.pin().unwrap();
    assert!(!page.swap_out());
    pin.zero_from(4);
    drop(pin);
    assert!(page.swap_out());
    assert!(page.pin_resident().unwrap().is_none());
    assert!(!reference.is_installed());
    let pin = page.pin().unwrap();
    assert!(!reference.installed_on(&pin));
    let mut bytes = [0; 8];
    pin.read(0, &mut bytes);
    assert_eq!(&bytes, b"orig\0\0\0\0");
}

#[test]
fn draining_retained_slot_is_not_reused_and_last_reference_drop_releases_it() {
    let storage = environment();
    let (page, first) = mapped(false);
    let second = first.clone();
    revoke_count(2);
    assert!(page.swap_out());
    drop(page.pin().unwrap());
    storage.active.store(false, Ordering::Relaxed);
    assert!(!page.swap_out());
    assert_eq!(storage.reserved.load(Ordering::Relaxed), 1);
    drop(first);
    assert_eq!(storage.live_slots.load(Ordering::Relaxed), 1);
    drop(second);
    assert_eq!(storage.live_slots.load(Ordering::Relaxed), 0);
}

#[test]
fn retained_swapcache_is_rewritten_from_latest_bytes_and_failed_rewrite_keeps_resident_authoritative()
 {
    let storage = environment();
    let (page, mut first) = mapped(false);
    let mut second = first.clone();
    revoke_count(2);
    assert!(page.swap_out());
    let pin = page.pin().unwrap();
    first.mark_installed(&pin);
    pin.write(0, b"modified");
    drop(pin);
    storage.fail_write.store(true, Ordering::Relaxed);
    assert!(!page.swap_out());
    assert!(first.is_installed() && !second.is_installed());
    assert_eq!(storage.live_slots.load(Ordering::Relaxed), 1);
    let mut bytes = [0; 8];
    page.pin().unwrap().read(0, &mut bytes);
    assert_eq!(&bytes, b"modified");
    storage.fail_write.store(false, Ordering::Relaxed);
    assert!(page.swap_out());
    assert_eq!(storage.reserved.load(Ordering::Relaxed), 1);
    assert!(!first.is_installed() && !second.is_installed());
    let pin = page.pin().unwrap();
    pin.read(0, &mut bytes);
    assert_eq!(&bytes, b"modified");
    first.mark_installed(&pin);
    second.mark_installed(&pin);
    assert_eq!(storage.live_slots.load(Ordering::Relaxed), 0);
}

#[test]
fn private_discard_drops_only_its_alias_then_frees_the_last_frame_despite_a_candidate_arc() {
    environment();
    let (page, first) = mapped(true);
    let second = first.clone();
    assert!(!first.discard());
    let mut bytes = [0; 8];
    page.pin().unwrap().read(0, &mut bytes);
    assert_eq!(&bytes, b"original");
    assert!(second.discard());
    assert!(matches!(
        page.pin(),
        Err(memory::SharedFileError::BeyondEof)
    ));
    assert!(!page.swap_out());
}

#[test]
fn discarding_last_stale_alias_releases_swapcache_while_installed_sibling_keeps_content() {
    let storage = environment();
    let (page, mut first) = mapped(true);
    let second = first.clone();
    revoke_count(2);
    assert!(page.swap_out());
    let pin = page.pin().unwrap();
    first.mark_installed(&pin);
    drop(pin);
    assert_eq!(storage.live_slots.load(Ordering::Relaxed), 1);
    assert!(!second.discard());
    assert!(first.is_installed() && first.private_exclusive());
    assert_eq!(storage.live_slots.load(Ordering::Relaxed), 0);
    let mut bytes = [0; 8];
    page.pin().unwrap().read(0, &mut bytes);
    assert_eq!(&bytes, b"original");
}

#[test]
fn unmapped_storage_is_restored_by_production_weak_page_registry() {
    let storage = environment();
    let page = SwappablePage::allocate_shared().unwrap();
    page.pin().unwrap().write(0, b"unmapped");
    assert!(page.swap_out());
    assert_eq!(storage.live_slots.load(Ordering::Relaxed), 1);
    storage.active.store(false, Ordering::Relaxed);
    memory::swappable_page::restore_storage(storage.identity, &mut || {
        Ok::<(), memory::SharedFileError>(())
    })
    .unwrap();
    assert_eq!(storage.live_slots.load(Ordering::Relaxed), 0);
    let mut bytes = [0; 8];
    page.pin().unwrap().read(0, &mut bytes);
    assert_eq!(&bytes, b"unmapped");
}

#[test]
fn owner_frame_arc_page_arc_and_registry_node_oom_retire_every_unpublished_frame() {
    use crate::allocation::{FailNth, LIVE_FRAMES, isolated};
    if isolated(
        "memory::tests::owner_frame_arc_page_arc_and_registry_node_oom_retire_every_unpublished_frame",
    ) {
        return;
    }
    assert_eq!(LIVE_FRAMES.load(Ordering::Relaxed), 0);
    // With a fresh registry the three production allocations are frame Arc, Page Arc and AVL node.
    for ordinal in 1..=3 {
        let frame = memory::SharedFrame::allocate().unwrap();
        let failure = FailNth::new(ordinal);
        let result = SwappablePage::new(frame, true);
        drop(failure);
        assert!(matches!(result, Err(memory::SharedFileError::OutOfMemory)));
        assert_eq!(LIVE_FRAMES.load(Ordering::Relaxed), 0);
    }
    let page = SwappablePage::allocate_shared().unwrap();
    assert_eq!(LIVE_FRAMES.load(Ordering::Relaxed), 1);
    drop(page);
    assert_eq!(LIVE_FRAMES.load(Ordering::Relaxed), 0);
}

#[test]
fn page_in_arc_oom_after_successful_io_keeps_slot_and_successful_swap_out_frees_frame() {
    use crate::allocation::{FailNth, LIVE_FRAMES, isolated};
    if isolated(
        "memory::tests::page_in_arc_oom_after_successful_io_keeps_slot_and_successful_swap_out_frees_frame",
    ) {
        return;
    }
    let storage = environment();
    let (page, mut reference) = mapped(true);
    assert_eq!(LIVE_FRAMES.load(Ordering::Relaxed), 1);
    revoke_count(1);
    assert!(page.swap_out());
    assert_eq!(LIVE_FRAMES.load(Ordering::Relaxed), 0);
    assert_eq!(memory::swappable_page::cached_pages(), 0);
    FRAME_OOM.with(|value| value.set(true));
    assert!(matches!(
        page.pin(),
        Err(memory::SharedFileError::OutOfMemory)
    ));
    assert_eq!(memory::swappable_page::cached_pages(), 0);
    let failure = FailNth::new(1);
    let result = page.pin();
    drop(failure);
    assert!(matches!(result, Err(memory::SharedFileError::OutOfMemory)));
    assert_eq!(LIVE_FRAMES.load(Ordering::Relaxed), 0);
    assert_eq!(memory::swappable_page::cached_pages(), 0);
    assert_eq!(storage.live_slots.load(Ordering::Relaxed), 1);
    assert!(!reference.is_installed());
    let pin = page.pin().unwrap();
    reference.mark_installed(&pin);
    assert_eq!(LIVE_FRAMES.load(Ordering::Relaxed), 1);
    assert_eq!(storage.live_slots.load(Ordering::Relaxed), 0);
    drop(pin);
    assert!(reference.discard());
    assert_eq!(LIVE_FRAMES.load(Ordering::Relaxed), 0);
}

#[test]
fn draining_page_snapshot_does_not_wait_for_unrelated_new_content_owners() {
    if crate::allocation::isolated(
        "memory::tests::draining_page_snapshot_does_not_wait_for_unrelated_new_content_owners",
    ) {
        return;
    }
    use std::{sync::mpsc, time::Duration};
    let storage = environment();
    let page = SwappablePage::allocate_shared().unwrap();
    page.pin().unwrap().write(0, b"retained");
    assert!(page.swap_out());
    storage.active.store(false, Ordering::Relaxed);
    let identity = storage.identity;
    let (new_pinned, new_ready) = mpsc::channel();
    let (release, released) = mpsc::channel();
    let (drained, completion) = mpsc::channel();
    let drainer = std::thread::spawn(move || {
        let (thread_started, started) = mpsc::channel();
        crate::READ_HOOK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                let unrelated = SwappablePage::allocate_shared().unwrap();
                let holder = std::thread::spawn(move || {
                    let _pin = unrelated.pin().unwrap();
                    thread_started.send(()).unwrap();
                    released.recv().unwrap();
                });
                started.recv().unwrap();
                new_pinned.send(holder).unwrap();
            }))
        });
        let result = memory::swappable_page::restore_storage(identity, &mut || {
            Ok::<(), memory::SharedFileError>(())
        });
        drained.send(result).unwrap();
    });
    let holder = new_ready.recv_timeout(Duration::from_secs(2)).unwrap();
    // Old generation reloop would wait on this new owner's PageOp even though it cannot hold
    // a Draining slot. Release on timeout as well, so a regression terminates rather than hangs.
    let result = completion.recv_timeout(Duration::from_secs(1));
    release.send(()).unwrap();
    holder.join().unwrap();
    drainer.join().unwrap();
    assert_eq!(result.unwrap(), Ok(()));
    assert_eq!(storage.live_slots.load(Ordering::Relaxed), 0);
    let mut bytes = [0; 8];
    page.pin().unwrap().read(0, &mut bytes);
    assert_eq!(&bytes, b"retained");
}

#[test]
fn swapcache_projection_counts_one_resident_owner_and_preserves_failure_and_drop_paths() {
    if crate::allocation::isolated(
        "memory::tests::swapcache_projection_counts_one_resident_owner_and_preserves_failure_and_drop_paths",
    ) {
        return;
    }
    use memory::swappable_page::cached_pages;
    let storage = environment();
    let (page, mut first) = mapped(true);
    let second = first.clone();
    revoke_count(2);
    assert_eq!(cached_pages(), 0);
    assert!(page.swap_out());
    assert_eq!(cached_pages(), 0);
    storage.fail_read.store(true, Ordering::Relaxed);
    assert!(page.pin().is_err());
    assert_eq!(cached_pages(), 0);
    storage.fail_read.store(false, Ordering::Relaxed);
    let pin = page.pin().unwrap();
    assert_eq!(cached_pages(), 1);
    first.mark_installed(&pin);
    assert_eq!(cached_pages(), 1);
    drop(pin);
    drop(page.pin().unwrap());
    assert_eq!(cached_pages(), 1, "pins do not create another cache owner");
    storage.fail_write.store(true, Ordering::Relaxed);
    assert!(!page.swap_out());
    assert_eq!(cached_pages(), 1);
    storage.fail_write.store(false, Ordering::Relaxed);
    assert!(page.swap_out());
    assert_eq!(
        cached_pages(),
        0,
        "replacing Resident retires its single cache capability"
    );
    let pin = page.pin().unwrap();
    assert_eq!(cached_pages(), 1);
    first.mark_installed(&pin);
    drop(pin);
    drop(second);
    assert_eq!(
        cached_pages(),
        0,
        "last stale alias Drop releases retained cache"
    );
    assert_eq!(storage.live_slots.load(Ordering::Relaxed), 0);
    assert!(first.discard());
    drop(page);
    assert_eq!(cached_pages(), 0);
}
