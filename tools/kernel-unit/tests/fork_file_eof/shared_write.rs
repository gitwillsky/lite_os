use super::*;
use core::sync::atomic::Ordering;

#[test]
fn freeze_retires_shared_writers_and_fork_recovery_reacquires_for_each_new_writable_leaf() {
    let (mut parent, backing, pages) = populate(true, false, 0);
    backing.writable.store(false, Ordering::Release);
    parent.revoke_shared_file_writers(backing.id());
    for (&vpn, resident) in &parent
        .areas
        .first_key_value()
        .unwrap()
        .1
        .shared_file
        .as_ref()
        .unwrap()
        .resident
    {
        assert!(!resident.writer);
        assert!(
            !parent
                .page_table
                .translate(vpn)
                .unwrap()
                .permissions()
                .contains(PagePermissions::WRITE)
        );
    }
    assert!(
        pages
            .iter()
            .all(|page| page.writers.load(Ordering::Relaxed) == 0)
    );
    let mut frozen_child = MemorySet::empty();
    parent.try_clone_for_fork(&mut frozen_child).unwrap();
    for (&vpn, resident) in &frozen_child
        .areas
        .first_key_value()
        .unwrap()
        .1
        .shared_file
        .as_ref()
        .unwrap()
        .resident
    {
        assert!(!resident.writer);
        assert!(
            !frozen_child
                .page_table
                .translate(vpn)
                .unwrap()
                .permissions()
                .contains(PagePermissions::WRITE)
        );
    }
    backing.writable.store(true, Ordering::Release);
    let mut recovered_child = MemorySet::empty();
    parent.try_clone_for_fork(&mut recovered_child).unwrap();
    for (&vpn, resident) in &recovered_child
        .areas
        .first_key_value()
        .unwrap()
        .1
        .shared_file
        .as_ref()
        .unwrap()
        .resident
    {
        assert!(resident.writer);
        assert!(
            recovered_child
                .page_table
                .translate(vpn)
                .unwrap()
                .permissions()
                .contains(PagePermissions::WRITE)
        );
    }
    assert!(
        pages
            .iter()
            .all(|page| page.writers.load(Ordering::Relaxed) == 1)
    );
    drop(recovered_child);
    assert!(
        pages
            .iter()
            .all(|page| page.writers.load(Ordering::Relaxed) == 0)
    );
}

#[test]
fn post_mprotect_fence_uses_actual_leaf_when_freeze_changes_during_the_transaction() {
    let (mut memory, backing, pages) = populate(true, false, 0);
    let range = memory.areas.first_key_value().unwrap().1.vpn_range.clone();
    // W leaves have already been committed by mprotect; freeze changes independently before
    // its post-fence claim retirement pass while the mm is still held by this transaction.
    backing.writable.store(false, Ordering::Release);
    memory.release_restricted_shared_writers(range.start, range.end);
    assert!(
        pages
            .iter()
            .all(|page| page.writers.load(Ordering::Relaxed) == 1)
    );
    // Only the real revoke/fence can retire them, even though VMA permissions still request W.
    memory.revoke_shared_file_writers(backing.id());
    assert!(
        pages
            .iter()
            .all(|page| page.writers.load(Ordering::Relaxed) == 0)
    );
}
