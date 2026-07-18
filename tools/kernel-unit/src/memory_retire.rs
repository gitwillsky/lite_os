#[path = "../../../kernel/src/memory/retire.rs"]
mod production;

use alloc::{sync::Arc, vec::Vec};
use spin::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Event {
    ClearPte,
    FenceComplete,
    FenceFailed,
    OwnerDrop,
}

struct Owner {
    events: Arc<Mutex<Vec<Event>>>,
}

impl Drop for Owner {
    fn drop(&mut self) {
        self.events.lock().push(Event::OwnerDrop);
    }
}

#[test]
fn owner_drop_follows_pte_clear_and_completed_fence() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let owner = Owner {
        events: events.clone(),
    };
    let clear_events = events.clone();
    let fence_events = events.clone();

    let owner = production::revoke_and_synchronize(
        owner,
        move |_| clear_events.lock().push(Event::ClearPte),
        move |_| {
            fence_events.lock().push(Event::FenceComplete);
            Ok::<(), ()>(())
        },
    )
    .unwrap();
    drop(owner);

    assert_eq!(
        events.lock().as_slice(),
        [Event::ClearPte, Event::FenceComplete, Event::OwnerDrop]
    );
}

#[test]
fn failed_fence_leaks_owner_instead_of_running_drop() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let owner = Owner {
        events: events.clone(),
    };
    let clear_events = events.clone();
    let fence_events = events.clone();

    let result = production::revoke_and_synchronize(
        owner,
        move |_| clear_events.lock().push(Event::ClearPte),
        move |_| {
            fence_events.lock().push(Event::FenceFailed);
            Err::<(), _>(())
        },
    );

    assert!(result.is_err());
    assert_eq!(
        events.lock().as_slice(),
        [Event::ClearPte, Event::FenceFailed]
    );
}

#[test]
fn release_recounts_owner_after_fence_without_exceeding_target() {
    let shared_at_revoke = Arc::new(());
    let concurrent_owner = shared_at_revoke.clone();
    assert_eq!(Arc::strong_count(&shared_at_revoke), 2);

    let unique_at_release = production::revoke_and_synchronize(
        shared_at_revoke,
        |_| {},
        move |_| {
            // 模拟另一 mm 在 shootdown fence 完成前撤销并释放同一 COW frame owner。
            drop(concurrent_owner);
            Ok::<(), ()>(())
        },
    )
    .unwrap();
    assert_eq!(Arc::strong_count(&unique_at_release), 1);

    let target = 1;
    let first =
        production::reclaim_release_decision(0, target, Arc::strong_count(&unique_at_release));
    assert!(first.release);
    assert!(first.reclaimed);
    let reclaimed = usize::from(first.reclaimed);
    drop(unique_at_release);

    // revoke 时另一个唯一页已被选中；release 重放必须在 target 达成后保留它，
    // 否则 2→1 的并发变化会把 owner result 从一页放大到两页。
    let selected_unique = Arc::new(());
    let second = production::reclaim_release_decision(
        reclaimed,
        target,
        Arc::strong_count(&selected_unique),
    );
    assert!(!second.release);
    assert!(!second.reclaimed);
    assert_eq!(reclaimed, target);
}
