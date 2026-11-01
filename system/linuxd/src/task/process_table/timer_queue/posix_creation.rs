use super::preparation_policy::{PosixCreateAction, posix_create_action};
use super::{PreparedPosixCreate, TimerError, TimerQueue};

impl TimerQueue {
    pub(super) fn next_posix_id(&self, tgid: usize) -> Result<i32, TimerError> {
        let mut id = 0i32;
        while self.posix_timers.contains_key(&(tgid, id)) {
            id = id.checked_add(1).ok_or(TimerError::Exhausted)?;
        }
        Ok(id)
    }

    pub(super) fn commit_posix_create(&mut self, prepared: &mut PreparedPosixCreate) -> bool {
        match posix_create_action(self.posix_timers.contains_key(&prepared.key)) {
            PosixCreateAction::RetargetPreparedNode => return false,
            PosixCreateAction::Commit => {}
        }
        self.posix_timers
            .commit_vacant(prepared.timer_node.take().expect("prepared POSIX node"));
        true
    }
}
