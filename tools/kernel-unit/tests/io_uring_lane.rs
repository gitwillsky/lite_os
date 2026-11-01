#![feature(allocator_ext)]
#![allow(dead_code)]
//! Actual lane FIFO/idle handoff; host wait publication injects submit after the queue-empty check.
extern crate alloc;
use std::sync::{
    Arc, Mutex as HostMutex, OnceLock,
    atomic::{AtomicUsize, Ordering},
};

static NOTIFICATIONS: AtomicUsize = AtomicUsize::new(0);
static WAITS: AtomicUsize = AtomicUsize::new(0);
static TASK: OnceLock<Arc<task::TaskControlBlock>> = OnceLock::new();
static SECOND: HostMutex<Option<Arc<lane::Ticket>>> = HostMutex::new(None);
static WORKER: HostMutex<Option<std::thread::JoinHandle<bool>>> = HostMutex::new(None);
struct Completed;
struct Notify;
impl fs::CompletionNotify for Notify {
    fn notify(&self) {
        NOTIFICATIONS.fetch_add(1, Ordering::SeqCst);
    }
}
mod fs {
    pub(crate) trait CompletionNotify: Send + Sync {
        fn notify(&self);
    }
}
mod sync {
    pub(crate) use spin::{Mutex, Once};
}
mod ipc {
    use alloc::sync::Arc;
    pub(crate) struct Pipe;
    pub(crate) struct PipeEnd(Arc<Pipe>);
    impl Pipe {
        pub(crate) fn notification_pair() -> Result<(Arc<PipeEnd>, Arc<PipeEnd>), ()> {
            let source = Arc::new(Self);
            Ok((Arc::new(PipeEnd(source.clone())), Arc::new(PipeEnd(source))))
        }
    }
    impl PipeEnd {
        pub(crate) fn pipe(&self) -> Arc<Pipe> {
            self.0.clone()
        }
        pub(crate) fn signal_readiness(&self) {}
        pub(crate) fn drain_readiness(&self) {}
    }
}
mod syscall {
    pub(crate) mod poll {
        use alloc::{sync::Arc, vec::Vec};
        pub(crate) struct PollWaitKeys;
        pub(crate) struct Guards;
        impl PollWaitKeys {
            pub(crate) fn new() -> Self {
                Self
            }
            pub(crate) fn add_notification(&mut self, _: &Arc<crate::ipc::Pipe>) -> Result<(), ()> {
                Ok(())
            }
            pub(crate) fn finish(self) -> (Vec<()>, Guards) {
                (Vec::new(), Guards)
            }
        }
        impl Guards {
            pub(crate) fn changed(&self) -> bool {
                false
            }
        }
    }
}
mod task {
    use super::*;
    pub(crate) struct TaskControlBlock;
    pub(crate) enum WaitResult {
        Woken,
    }
    pub(crate) fn spawn_kernel_thread(
        _: &'static str,
        body: alloc::boxed::Box<dyn FnOnce() + Send>,
    ) -> Result<(), ()> {
        *WORKER.lock().unwrap() = Some(std::thread::spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(body));
            result.is_err_and(|error| error.is::<Completed>())
        }));
        Ok(())
    }
    pub(crate) fn wait_for_poll(
        _: alloc::vec::Vec<()>,
        _: Option<u64>,
        ready: impl FnOnce() -> bool,
    ) -> WaitResult {
        if WAITS.fetch_add(1, Ordering::SeqCst) == 0 {
            // Notification arrives after lane marked itself idle and before wait publication.
            // Its edge has no armed listener, so only the actual owner recheck can close this window.
            *SECOND.lock().unwrap() = Some(
                lane::submit(
                    alloc::boxed::Box::new(|_| 222),
                    Arc::downgrade(TASK.get().unwrap()),
                    Arc::new(Notify),
                )
                .unwrap(),
            );
            assert!(
                ready(),
                "claimed lane worker slept with a queued job after an early notification"
            );
            WaitResult::Woken
        } else {
            assert_eq!(
                NOTIFICATIONS.load(Ordering::SeqCst),
                2,
                "both actual jobs must execute"
            );
            assert!(
                !ready(),
                "an unclaimed idle worker must be allowed to sleep"
            );
            std::panic::resume_unwind(alloc::boxed::Box::new(Completed));
        }
    }
}
#[path = "../../../system/linuxd/src/syscall/io_uring/lane.rs"]
mod lane;

#[test]
fn submit_between_idle_publication_and_sleep_is_rechecked_by_the_actual_pool() {
    let task = TASK.get_or_init(|| Arc::new(task::TaskControlBlock));
    lane::start().unwrap();
    let first = lane::submit(
        alloc::boxed::Box::new(|_| 111),
        Arc::downgrade(task),
        Arc::new(Notify),
    )
    .unwrap();
    let worker = WORKER.lock().unwrap().take().unwrap();
    assert!(
        worker.join().unwrap(),
        "worker stopped before consuming its claimed handoff"
    );
    assert_eq!(first.take(), Some(111));
    assert_eq!(SECOND.lock().unwrap().take().unwrap().take(), Some(222));
}
