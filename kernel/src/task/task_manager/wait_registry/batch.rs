use super::*;
use crate::fallible_tree::FallibleMap;

pub(in crate::task::task_manager) struct ClaimedBatch {
    entries: FallibleMap<WaitIndexKey, Arc<WaitRegistration>>,
}

impl ClaimedBatch {
    pub(in crate::task::task_manager) const fn new() -> Self {
        Self {
            entries: FallibleMap::new(),
        }
    }

    pub(in crate::task::task_manager) fn push(&mut self, mut claimed: ClaimedWait) {
        self.entries.commit_vacant(
            claimed
                .staging
                .take()
                .expect("claimed wait lost its staging node"),
        );
    }

    pub(in crate::task::task_manager) fn pop(&mut self) -> Option<ClaimedWait> {
        let key = *self.entries.first_key_value()?.0;
        let registration = self.entries.remove(&key)?;
        Some(ClaimedWait {
            id: registration.id,
            task: registration.task.clone(),
            kind: *registration.kind.lock(),
            staging: None,
        })
    }
}
