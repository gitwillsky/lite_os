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

/// 用有序 resident 集模拟 `next_private_resident_from`：revoke 阶段不移除 owner，
/// replay 阶段按 reclaim 语义移除已扫描页，两阶段共享同一 walk 状态机。
mod reclaim_walk {
    use super::production::PrivateReclaimWalk;
    use std::collections::BTreeSet;

    fn next_resident_from(residents: &BTreeSet<usize>, cursor: usize) -> Option<usize> {
        residents.range(cursor..).next().copied()
    }

    /// revoke：扫描全部预算内 resident，cursor 只随实际扫描页推进。
    fn revoke(
        residents: &BTreeSet<usize>,
        initial: usize,
        scan_budget: usize,
    ) -> (usize, Vec<usize>) {
        let mut walk = PrivateReclaimWalk::new(initial);
        let mut scanned = Vec::new();
        while scanned.len() < scan_budget {
            let Some(vpn) = next_resident_from(residents, walk.probe()) else {
                if !walk.wrap_or_finish() {
                    break;
                }
                continue;
            };
            if !walk.advance(vpn) {
                break;
            }
            scanned.push(vpn);
        }
        (walk.committed(), scanned)
    }

    /// replay：重放 scanned 序列并按 reclaim 语义移除 owner，终值必须等于 revoke committed。
    fn replay(residents: &BTreeSet<usize>, initial: usize, scanned: &[usize]) -> usize {
        let mut remaining = residents.clone();
        let mut walk = PrivateReclaimWalk::new(initial);
        for &expected in scanned {
            let vpn = loop {
                match next_resident_from(&remaining, walk.probe()) {
                    Some(vpn) => break vpn,
                    None => assert!(walk.wrap_or_finish()),
                }
            };
            assert!(walk.advance(vpn));
            assert_eq!(vpn, expected);
            remaining.remove(&vpn);
        }
        walk.committed()
    }

    #[test]
    fn wrap_lookahead_does_not_clobber_committed_cursor() {
        // panic 现场布局：initial 之后只有高地址 resident，最高页在地址空间顶部
        // （栈顶 0x3fffffe）。扫描越过顶页后 wrap 回看低地址，发现最小 resident 已不
        // 小于 initial 而结束；旧实现把 wrap 的 0 提交为 final cursor，与 replay 停在
        // after(0x3fffffe)=0x3ffffff 冲突。
        let residents = BTreeSet::from([0x20, 0x3f_ffffe]);
        let (committed, scanned) = revoke(&residents, 0x15, 16);
        assert_eq!(scanned, [0x20, 0x3f_ffffe]);
        assert_eq!(committed, 0x3f_fffff);
        assert_eq!(replay(&residents, 0x15, &scanned), committed);
    }

    #[test]
    fn wrap_then_low_pages_commit_after_last_scanned() {
        // wrap 后继续扫描低地址页：committed 必须落在最后一个低地址扫描页之后，
        // 而不是停留在 wrap 的 0 或高地址页之后。
        let residents = BTreeSet::from([0x5, 0x20, 0x30]);
        let (committed, scanned) = revoke(&residents, 0x15, 16);
        assert_eq!(scanned, [0x20, 0x30, 0x5]);
        assert_eq!(committed, 0x6);
        assert_eq!(replay(&residents, 0x15, &scanned), committed);
    }

    #[test]
    fn scan_budget_stop_commits_after_last_scanned() {
        let residents = BTreeSet::from([0x10, 0x11, 0x12, 0x13]);
        let (committed, scanned) = revoke(&residents, 0x10, 2);
        assert_eq!(scanned, [0x10, 0x11]);
        assert_eq!(committed, 0x12);
        assert_eq!(replay(&residents, 0x10, &scanned), committed);
    }

    #[test]
    fn zero_initial_scans_resident_set_once() {
        // initial 为 0 时 wrap 后任何 resident 都触发回绕终止；整集只扫一圈。
        let residents = BTreeSet::from([0x3, 0x7, 0x3f_ffffe]);
        let (committed, scanned) = revoke(&residents, 0, 16);
        assert_eq!(scanned, [0x3, 0x7, 0x3f_ffffe]);
        assert_eq!(committed, 0x3f_fffff);
        assert_eq!(replay(&residents, 0, &scanned), committed);
    }

    #[test]
    fn empty_resident_set_keeps_initial_cursor() {
        let residents = BTreeSet::new();
        let (committed, scanned) = revoke(&residents, 0x42, 16);
        assert!(scanned.is_empty());
        assert_eq!(committed, 0x42);
    }
}
