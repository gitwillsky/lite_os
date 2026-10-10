use super::*;

impl WaitRegistry {
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
            match self.notify(&registration, WaitResult::Woken) {
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

    pub(in crate::task::process_table) fn wake_task_event(
        &self,
        key: crate::sync::TaskWaitKey,
        result: WaitResult,
    ) -> Option<SourceWake> {
        let (owner, ticket) = key.event_identity();
        let source = WaitIndexKey::TaskEvent { owner, ticket };
        let (candidate, registration) = self.source_candidate(source, None)?;
        if candidate != source {
            return None;
        }
        assert!(
            matches!(*registration.kind.lock(), IndexedWaitKind::TaskEvent(current) if current == key)
        );
        match self.notify(&registration, result) {
            NotifyOutcome::BeforeArm => Some(SourceWake {
                claimed: None,
                group: None,
            }),
            NotifyOutcome::Armed(claimed) => Some(SourceWake {
                claimed: Some(claimed),
                group: None,
            }),
            NotifyOutcome::Stale => None,
        }
    }

    pub(in crate::task::process_table) fn wake_signal_registration(
        &self,
        task: &Arc<crate::task::TaskControlBlock>,
    ) -> Option<SourceWake> {
        let lower = WaitIndexKey::Task {
            tid: task.tid(),
            id: 0,
        };
        let mut cursor = None;
        loop {
            let (index, registration) = self.source_candidate(lower, cursor)?;
            let WaitIndexKey::Task { tid, .. } = index else {
                return None;
            };
            if tid != task.tid() {
                return None;
            }
            cursor = Some(index);
            let IndexedWaitKind::Signal { mask } = *registration.kind.lock() else {
                continue;
            };
            if task.with_pending_signal(mask, || ()).is_none() {
                continue;
            }
            match self.notify(&registration, WaitResult::Woken) {
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

    pub(in crate::task::process_table) fn interrupt_task(
        &self,
        task: &Arc<crate::task::TaskControlBlock>,
    ) -> Option<SourceWake> {
        let lower = WaitIndexKey::Task {
            tid: task.tid(),
            id: 0,
        };
        let mut cursor = None;
        loop {
            let (index, registration) = self.source_candidate(lower, cursor)?;
            let WaitIndexKey::Task { tid, .. } = index else {
                return None;
            };
            if tid != task.tid() {
                return None;
            }
            cursor = Some(index);
            match self.notify(&registration, WaitResult::Interrupted) {
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
