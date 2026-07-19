use alloc::sync::Arc;

use super::{TaskControlBlock, WaitMembership, WaitResult};
use crate::sync::{TaskMutexWaitKey, TaskMutexWaitTarget, WaitCompletion};

pub(in crate::task) fn initialize() {
    crate::sync::install_task_mutex_wait_target_factory(current_wait_target);
}

fn current_wait_target() -> Option<Arc<dyn TaskMutexWaitTarget>> {
    crate::task::current_task().map(|task| task as Arc<dyn TaskMutexWaitTarget>)
}

impl TaskMutexWaitTarget for TaskControlBlock {
    fn sleep(self: Arc<Self>, completion: &WaitCompletion, key: TaskMutexWaitKey) {
        if !completion.begin_arming() {
            return;
        }
        let prepared = super::context_switch::prepare_current_block(&self, (), |_, _| {
            WaitMembership::TaskMutex(key)
        });
        if completion.finish_arming() {
            assert!(crate::task::processor::wake_waiting_task(
                self.clone(),
                WaitMembership::TaskMutex(key),
                Some(WaitResult::Woken),
            ));
        }
        assert_eq!(prepared.suspend(), WaitResult::Woken);
    }

    fn wake(self: Arc<Self>, key: TaskMutexWaitKey) {
        assert!(crate::task::processor::wake_waiting_task(
            self,
            WaitMembership::TaskMutex(key),
            Some(WaitResult::Woken),
        ));
    }
}
