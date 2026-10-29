//! Actual FIFO mutex/event owners; Target is the scheduler/signal seam.
use crate::execution::sync::{TaskEvent, TaskMutex, TaskMutexLockError, WaitResult};
use alloc::sync::Arc;
#[path = "interruptible_event_target.rs"]
#[allow(dead_code, reason = "epoch capture 只在独立 task_event 回归使用")]
mod target;
use target::Target;

#[test]
fn killable_event_ignores_ordinary_signal_and_cancels_on_fatal_signal() {
    let event = Arc::new(TaskEvent::new());
    let target = Target::new();
    let worker_event = event.clone();
    let worker_target = target.clone();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let _binding = worker_target.bind();
        done_tx.send(worker_event.wait_killable()).unwrap();
    });
    target.wait_until_sleeping();
    target.signal();
    assert!(matches!(
        done_rx.try_recv(),
        Err(std::sync::mpsc::TryRecvError::Empty)
    ));
    target.fatal_signal();
    assert_eq!(
        done_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap(),
        WaitResult::Interrupted
    );
    worker.join().unwrap();
    target.clear_signal();
    let _binding = target.bind();
    event.signal();
    assert_eq!(event.wait_killable(), WaitResult::Woken);
}

#[test]
fn cancelling_middle_mutex_waiter_keeps_fifo_neighbors_and_releases_queue_metadata() {
    let mutex = Arc::new(TaskMutex::new(0));
    let owner = mutex.lock().unwrap();
    let first_target = Target::new();
    let first_mutex = mutex.clone();
    let first_binding = first_target.clone();
    let first = std::thread::spawn(move || {
        let _binding = first_binding.bind();
        *first_mutex.lock_killable().unwrap() += 1;
    });
    first_target.wait_until_sleeping();
    let middle_target = Target::new();
    let middle_mutex = mutex.clone();
    let middle_binding = middle_target.clone();
    let middle = std::thread::spawn(move || {
        let _binding = middle_binding.bind();
        matches!(
            middle_mutex.lock_killable(),
            Err(TaskMutexLockError::Interrupted)
        )
    });
    middle_target.wait_until_sleeping();
    let last_target = Target::new();
    let last_mutex = mutex.clone();
    let last_binding = last_target.clone();
    let last = std::thread::spawn(move || {
        let _binding = last_binding.bind();
        let mut guard = last_mutex.lock_interruptible().unwrap();
        assert_eq!(*guard, 1);
        *guard += 10;
    });
    last_target.wait_until_sleeping();
    middle_target.fatal_signal();
    assert!(middle.join().unwrap());
    drop(owner);
    first.join().unwrap();
    last.join().unwrap();
    assert_eq!(*mutex.try_lock().unwrap(), 11);
    assert_eq!(Arc::strong_count(&first_target), 1);
    assert_eq!(Arc::strong_count(&middle_target), 1);
    assert_eq!(Arc::strong_count(&last_target), 1);
}

#[test]
fn fatal_cancellation_racing_handoff_never_strands_mutex_ownership() {
    for _ in 0..128 {
        let mutex = Arc::new(TaskMutex::new(0));
        let owner = mutex.lock().unwrap();
        let target = Target::new();
        let worker_target = target.clone();
        let worker_mutex = mutex.clone();
        let worker = std::thread::spawn(move || {
            let _binding = worker_target.bind();
            match worker_mutex.lock_killable() {
                Ok(mut guard) => {
                    *guard = 1;
                    true
                }
                Err(TaskMutexLockError::Interrupted) => false,
                Err(TaskMutexLockError::OutOfMemory) => panic!("unexpected allocation failure"),
            }
        });
        target.wait_until_sleeping();
        let signal_target = target.clone();
        let signal = std::thread::spawn(move || signal_target.fatal_signal());
        drop(owner);
        let entered = worker.join().unwrap();
        signal.join().unwrap();
        assert_eq!(*mutex.try_lock().unwrap(), usize::from(entered));
        assert_eq!(Arc::strong_count(&target), 1);
    }
}
