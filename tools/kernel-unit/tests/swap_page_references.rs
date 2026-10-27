//! 生产 canonical reference state；没有复制迁移代次、COW 或计数算法。
#[path = "../../../kernel/src/memory/swappable_page/references.rs"]
mod references;
use references::MappingState;

#[test]
fn one_of_two_aliases_page_in_does_not_charge_the_other_rss() {
    // installed_count 是 AnonPages/Mapped 的 Linux mapcount：换出提交即归零，逐个重装才恢复。
    let mut state = MappingState::default();
    let mut first = state.installed(true, state.epoch());
    let second = state.inherit(&first);
    assert!(!state.private_exclusive(&first));
    assert_eq!(state.installed_count(), 2);
    let snapshot = state.snapshot();
    assert_eq!(snapshot.total(), 2);
    assert!(state.commit_swap(snapshot, 2));
    assert_eq!(state.stale(), 2);
    assert_eq!(state.installed_count(), 0);
    state.mark_installed(&mut first, state.epoch());
    assert!(state.is_installed(&first));
    assert!(!state.is_installed(&second));
    assert_eq!(state.stale(), 1);
    assert_eq!(state.installed_count(), 1);
    state.release(second);
    assert_eq!(state.stale(), 0);
    assert_eq!(state.installed_count(), 1);
    assert!(state.private_exclusive(&first));
    state.release(first);
    assert_eq!(state.snapshot().total(), 0);
}

#[test]
fn missing_retiring_mm_keeps_the_old_epoch_after_early_leaf_revoke() {
    let mut state = MappingState::default();
    let live = state.installed(false, 0);
    let retiring = state.inherit(&live);
    let begin = state.snapshot();
    assert!(!state.commit_swap(begin, 1));
    assert!(state.is_installed(&live) && state.is_installed(&retiring));
    assert_eq!(state.epoch(), 0);
    state.release(retiring);
    // 此轮遗漏的 retirement 已完成，但集合变过，仍必须下一轮重新取证据。
    assert!(!state.commit_swap(begin, 1));
    assert!(state.commit_swap(state.snapshot(), 1));
    state.release(live);
}

#[test]
fn io_window_reference_change_cancels_even_when_the_total_returns_to_the_same_value() {
    let mut state = MappingState::default();
    let parent = state.installed(true, 0);
    let discarded = state.inherit(&parent);
    let begin = state.snapshot();
    state.release(discarded);
    let replacement = state.inherit(&parent);
    assert_eq!(state.snapshot().total(), begin.total());
    assert!(!state.commit_swap(begin, 2));
    assert_eq!(state.epoch(), 0);
    assert!(state.is_installed(&replacement));
    state.release(replacement);
    state.release(parent);
}

#[test]
fn inherited_swap_entry_and_prot_none_unuse_keep_one_storage_claim_until_every_record_installs() {
    let mut state = MappingState::default();
    let mut parent = state.installed(true, 0);
    assert!(state.commit_swap(state.snapshot(), 1));
    let mut child = state.inherit(&parent);
    assert_eq!(state.stale(), 2);
    // Page-in 不推进物理 migration epoch；两条 record 各自完成 logical install。
    let epoch = state.epoch();
    state.mark_installed(&mut parent, epoch);
    assert_eq!(state.stale(), 1);
    state.mark_installed(&mut child, epoch);
    assert_eq!(state.stale(), 0);
    state.mark_installed(&mut child, epoch);
    assert_eq!(state.stale(), 0);
    state.release(child);
    state.release(parent);
}

#[test]
fn shared_mapping_is_never_authorized_as_private_cow_exclusive() {
    let mut state = MappingState::default();
    let shared = state.installed(false, 0);
    assert!(!state.private_exclusive(&shared));
    state.release(shared);
}

#[test]
fn a_waiter_cannot_install_a_pre_wait_physical_epoch() {
    let mut state = MappingState::default();
    let mut mapping = state.installed(true, 0);
    assert!(state.commit_swap(state.snapshot(), 1));
    let failure = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        state.mark_installed(&mut mapping, 0);
    }));
    assert!(failure.is_err());
    assert!(!state.is_installed(&mapping));
    assert_eq!(state.stale(), 1);
    state.release(mapping);
}
