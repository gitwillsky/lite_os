use alloc::sync::Arc;

use super::{TaskControlBlock, WaitMembership, WaitResult};
use crate::sync::{TaskWaitKey, TaskWaitTarget, WaitCompletion};

pub(in crate::task) fn initialize() {
    crate::sync::install_wait_target_factory(current_wait_target);
}

fn current_wait_target() -> Option<Arc<dyn TaskWaitTarget>> {
    crate::task::current_task().map(|task| task as Arc<dyn TaskWaitTarget>)
}

impl TaskWaitTarget for TaskControlBlock {
    fn sleep(self: Arc<Self>, completion: &WaitCompletion, key: TaskWaitKey) {
        if !completion.begin_arming() {
            return;
        }
        let prepared = super::context_switch::prepare_current_block(&self, (), |_, _| {
            WaitMembership::TaskWait(key)
        });
        if completion.finish_arming() {
            assert!(crate::task::processor::wake_waiting_task(
                self.clone(),
                WaitMembership::TaskWait(key),
                Some(WaitResult::Woken),
            ));
        }
        assert_eq!(prepared.suspend(), WaitResult::Woken);
    }

    fn wake(self: Arc<Self>, key: TaskWaitKey) {
        assert!(crate::task::processor::wake_waiting_task(
            self,
            WaitMembership::TaskWait(key),
            Some(WaitResult::Woken),
        ));
    }
}
