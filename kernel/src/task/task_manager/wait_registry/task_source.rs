use super::*;

impl WaitRegistry {
    pub(in crate::task::task_manager) fn wake_signal_registration(
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

    pub(in crate::task::task_manager) fn interrupt_task(
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
