//! 一次 source 通知的登记边界，防止唤醒回调重入后追逐新一代等待。
use super::*;

/// 一次通知只消费快照之前分配的 ticket；不持锁，不复制 registration。
///
/// 1. producer 在通知任何 waiter 前取得边界；
/// 2. 每次 claim 仍由 registry 的状态机与跨 shard detach 串行；
/// 3. 被唤醒者可以立即重登记，但新 ticket 不属于旧 readiness 快照，不能被它再次唤醒。
pub(in crate::task::process_table) struct SourceNotification<'a> {
    pub(super) registry: &'a WaitRegistry,
    pub(super) last_ticket: u64,
}

impl SourceNotification<'_> {
    fn source_candidate(
        &self,
        lower: WaitIndexKey,
        cursor: Option<WaitIndexKey>,
    ) -> Option<(WaitIndexKey, Arc<WaitRegistration>)> {
        // 同一 source/mode 内按 ticket 排序；越过边界后本轮不可能再有候选。
        self.registry
            .source_candidate(lower, cursor)
            .filter(|(_, registration)| registration.id <= self.last_ticket)
    }

    pub(in crate::task::process_table) fn wake_advisory_one(
        &self,
        key: AdvisoryLockKey,
    ) -> Option<SourceWake> {
        let lower = WaitIndexKey::AdvisoryLock { key, id: 0 };
        let mut cursor = None;
        loop {
            let (index, registration) = self.source_candidate(lower, cursor)?;
            let WaitIndexKey::AdvisoryLock { key: candidate, .. } = index else {
                return None;
            };
            if candidate != key {
                return None;
            }
            cursor = Some(index);
            match self.registry.notify(&registration, WaitResult::Woken) {
                NotifyOutcome::BeforeArm => {
                    return Some(SourceWake {
                        claimed: None,
                        group: None,
                    });
                }
                NotifyOutcome::Armed(claimed) => {
                    return Some(SourceWake {
                        claimed: Some(claimed),
                        group: None,
                    });
                }
                NotifyOutcome::Stale => {}
            }
        }
    }

    pub(in crate::task::process_table) fn wake_console_one(
        &self,
        exclusive: bool,
        ready: i16,
        excluded_groups: &[Option<usize>],
    ) -> Option<SourceWake> {
        let lower = WaitIndexKey::Console { exclusive, id: 0 };
        let mut cursor = None;
        loop {
            let (index, registration) = self.source_candidate(lower, cursor)?;
            let WaitIndexKey::Console {
                exclusive: candidate,
                ..
            } = index
            else {
                return None;
            };
            if candidate != exclusive {
                return None;
            }
            cursor = Some(index);
            let Some(group) = WaitRegistry::console_group(&registration, ready) else {
                continue;
            };
            if group.is_some_and(|group| excluded_groups.contains(&Some(group))) {
                continue;
            }
            match self.registry.notify(&registration, WaitResult::Woken) {
                NotifyOutcome::BeforeArm => {
                    return Some(SourceWake {
                        claimed: None,
                        group,
                    });
                }
                NotifyOutcome::Armed(claimed) => {
                    return Some(SourceWake {
                        claimed: Some(claimed),
                        group,
                    });
                }
                NotifyOutcome::Stale => {}
            }
        }
    }

    pub(in crate::task::process_table) fn wake_pipe_one(
        &self,
        identity: usize,
        direction: PipeDirection,
        exclusive: bool,
        ready: i16,
        state: PipePollState,
        excluded_groups: &fallible_tree::FallibleMap<usize, ()>,
    ) -> Option<SourceWake> {
        let lower = WaitIndexKey::Pipe {
            identity,
            direction: pipe_direction(direction),
            exclusive,
            id: 0,
        };
        let mut cursor = None;
        loop {
            let (index, registration) = self.source_candidate(lower, cursor)?;
            let WaitIndexKey::Pipe {
                identity: candidate_identity,
                direction: candidate_direction,
                exclusive: candidate_exclusive,
                ..
            } = index
            else {
                return None;
            };
            if (candidate_identity, candidate_direction, candidate_exclusive)
                != (identity, pipe_direction(direction), exclusive)
            {
                return None;
            }
            cursor = Some(index);
            let Some(group) =
                WaitRegistry::pipe_group(&registration, identity, direction, ready, state)
            else {
                continue;
            };
            if group.is_some_and(|group| excluded_groups.contains_key(&group)) {
                continue;
            }
            match self.registry.notify(&registration, WaitResult::Woken) {
                NotifyOutcome::BeforeArm => {
                    return Some(SourceWake {
                        claimed: None,
                        group,
                    });
                }
                NotifyOutcome::Armed(claimed) => {
                    return Some(SourceWake {
                        claimed: Some(claimed),
                        group,
                    });
                }
                NotifyOutcome::Stale => {}
            }
        }
    }

    /// signalfd 的原生 sighand wait queue 唤醒全部直接 poll/read wait；不过滤 blocked mask。
    /// epoll 的 wake-one/exclusive 由持久 source index 和 epoll notification 自己处理。
    pub(in crate::task::process_table) fn wake_signal_source_one(
        &self,
        identity: usize,
    ) -> Option<SourceWake> {
        let lower = WaitIndexKey::SignalSource { identity, id: 0 };
        let mut cursor = None;
        loop {
            let (index, registration) = self.source_candidate(lower, cursor)?;
            let WaitIndexKey::SignalSource {
                identity: candidate,
                ..
            } = index
            else {
                return None;
            };
            if candidate != identity {
                return None;
            }
            cursor = Some(index);
            assert!(matches!(*registration.kind.lock(), IndexedWaitKind::Poll));
            match self.registry.notify(&registration, WaitResult::Woken) {
                NotifyOutcome::BeforeArm => {
                    return Some(SourceWake {
                        claimed: None,
                        group: None,
                    });
                }
                NotifyOutcome::Armed(claimed) => {
                    return Some(SourceWake {
                        claimed: Some(claimed),
                        group: None,
                    });
                }
                NotifyOutcome::Stale => {}
            }
        }
    }
}
