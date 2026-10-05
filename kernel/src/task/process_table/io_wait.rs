use alloc::sync::Arc;

use super::{TaskControlBlock, WaitMembership, WaitResult};
use crate::drivers::io_completion::{IoCompletion, IoWaitKey, IoWaitTarget};

pub(in crate::task) fn initialize_driver_io_wait() {
    crate::drivers::io_completion::install_wait_target_factory(current_io_wait_target);
}

fn current_io_wait_target() -> Option<Arc<dyn IoWaitTarget>> {
    crate::task::current_task().map(|task| task as Arc<dyn IoWaitTarget>)
}

impl IoWaitTarget for TaskControlBlock {
    fn sleep(self: Arc<Self>, completion: &IoCompletion, request: IoWaitKey) {
        if !completion.begin_arming() {
            return;
        }
        let prepared = super::context_switch::prepare_current_block(&self, (), |_, _| {
            WaitMembership::DriverIo(request)
        });
        if completion.finish_arming() {
            crate::task::processor::wake_waiting_task(
                self.clone(),
                WaitMembership::DriverIo(request),
                Some(WaitResult::Woken),
            );
        }
        assert_eq!(prepared.suspend(), WaitResult::Woken);
    }

    fn wake(self: Arc<Self>, request: IoWaitKey) {
        crate::task::processor::wake_waiting_task(
            self,
            WaitMembership::DriverIo(request),
            Some(WaitResult::Woken),
        );
    }
}
