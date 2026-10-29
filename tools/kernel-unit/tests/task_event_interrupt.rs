//! Production event/completion protocol with a host scheduler and deliverable-signal seam.
#![allow(dead_code)]
extern crate alloc;

#[path = "../src/interruptible_event_target.rs"]
mod target;
use crate::execution::sync::{TaskEvent, WaitResult};
use alloc::sync::Arc;
use target::Target;

#[test]
fn pending_signal_and_interrupted_wait_preserve_event_and_reusable_completion() {
    let target = Target::new();
    let _binding = target.bind();
    let event = TaskEvent::new();
    target.signal();
    assert_eq!(event.wait_interruptible(), WaitResult::Interrupted);
    event.signal();
    assert_eq!(event.wait_interruptible(), WaitResult::Woken);
    assert_eq!(event.wait_interruptible(), WaitResult::Interrupted);
    target.clear_signal();
    event.signal();
    event.wait();
}

#[test]
fn sleeping_interruption_and_late_event_wake_cannot_poison_the_next_ticket() {
    let event = Arc::new(TaskEvent::new());
    for _ in 0..128 {
        let target = Target::new();
        let worker_event = event.clone();
        let worker_target = target.clone();
        let worker = std::thread::spawn(move || {
            let _binding = worker_target.bind();
            worker_event.wait_interruptible()
        });
        target.wait_until_sleeping();
        target.signal();
        assert_eq!(worker.join().unwrap(), WaitResult::Interrupted);
        event.signal();
        let _binding = target.bind();
        target.clear_signal();
        event.wait();
    }
}

#[test]
fn simultaneous_signal_and_event_have_one_winner_and_leave_a_usable_event() {
    for _ in 0..128 {
        let target = Target::new();
        let event = Arc::new(TaskEvent::new());
        let worker_target = target.clone();
        let worker_event = event.clone();
        let worker = std::thread::spawn(move || {
            let _binding = worker_target.bind();
            worker_event.wait_interruptible()
        });
        target.wait_until_sleeping();
        let race = Arc::new(std::sync::Barrier::new(3));
        let signal_race = race.clone();
        let signal_target = target.clone();
        let signaler = std::thread::spawn(move || {
            signal_race.wait();
            signal_target.signal();
        });
        let event_race = race.clone();
        let event_source = event.clone();
        let notifier = std::thread::spawn(move || {
            event_race.wait();
            event_source.signal();
        });
        race.wait();
        signaler.join().unwrap();
        notifier.join().unwrap();
        assert!(matches!(
            worker.join().unwrap(),
            WaitResult::Woken | WaitResult::Interrupted
        ));
        let _binding = target.bind();
        target.clear_signal();
        event.signal();
        event.wait();
        event.signal();
        assert_eq!(event.wait_interruptible(), WaitResult::Woken);
    }
}

#[test]
fn task_mutex_style_uninterruptible_event_keeps_its_membership_until_completion() {
    let target = Target::new();
    let event = Arc::new(TaskEvent::new());
    let worker_target = target.clone();
    let worker_event = event.clone();
    let worker = std::thread::spawn(move || {
        let _binding = worker_target.bind();
        worker_event.wait();
    });
    target.wait_until_sleeping();
    target.signal();
    assert!(!worker.is_finished());
    event.signal();
    worker.join().unwrap();
}

#[test]
fn extracted_old_signal_wake_cannot_match_an_event_rebuilt_at_the_same_address() {
    use crate::execution::sync::TaskWaitTarget;
    let target = Target::new();
    let mut event = Arc::new(TaskEvent::new());
    let address = Arc::as_ptr(&event);
    let source = event.clone();
    let first_target = target.clone();
    let first = std::thread::spawn(move || {
        let _binding = first_target.bind();
        source.wait_interruptible()
    });
    target.wait_until_sleeping();
    let extracted_key = target.capture_wait_key();
    event.signal();
    assert_eq!(first.join().unwrap(), WaitResult::Woken);
    *Arc::get_mut(&mut event).unwrap() = TaskEvent::new();
    assert_eq!(Arc::as_ptr(&event), address);
    let source = event.clone();
    let second_target = target.clone();
    let second = std::thread::spawn(move || {
        let _binding = second_target.bind();
        source.wait_interruptible()
    });
    target.wait_until_sleeping();
    assert_ne!(target.capture_wait_key(), extracted_key);
    // The signal sender captured the old membership before dropping the scheduler lock.
    target.clone().wake(extracted_key);
    assert!(!second.is_finished());
    event.signal();
    assert_eq!(second.join().unwrap(), WaitResult::Woken);
}

#[test]
fn timeout_cancels_membership_and_late_signal_remains_for_the_next_wait() {
    let target = Target::new();
    let _binding = target.bind();
    let event = TaskEvent::new();
    assert_eq!(event.wait_interruptible_until(0), WaitResult::TimedOut);
    for _ in 0..16 {
        assert_eq!(
            event.wait_interruptible_until(target::monotonic_ns() + 1_000_000),
            WaitResult::TimedOut
        );
        event.signal();
        assert_eq!(event.wait_interruptible_until(0), WaitResult::Woken);
    }
    target.signal();
    assert_eq!(
        event.wait_interruptible_until(target::monotonic_ns() + 1_000_000_000),
        WaitResult::Interrupted
    );
    target.clear_signal();
    event.signal();
    event.wait();
}
#[test]
fn timed_event_wake_uses_exact_ticket_and_cannot_wake_later_wait() {
    let target = Target::new();
    let event = Arc::new(TaskEvent::new());
    let worker_target = target.clone();
    let worker_event = event.clone();
    let worker = std::thread::spawn(move || {
        let _binding = worker_target.bind();
        worker_event.wait_interruptible_until(target::monotonic_ns() + 5_000_000_000)
    });
    target.wait_until_sleeping();
    let key = target.capture_wait_key();
    assert!(key.deadline().is_some());
    event.signal();
    assert_eq!(worker.join().unwrap(), WaitResult::Woken);
    crate::execution::sync::TaskWaitTarget::wake(target.clone(), key);
    let _binding = target.bind();
    assert_eq!(event.wait_interruptible_until(0), WaitResult::TimedOut);
}

#[path = "."]
pub(crate) mod execution {
    #[path = "../../../kernel/src/execution/sync"]
    pub(crate) mod sync {
        mod task_event;
        mod task_wait;
        mod wait_completion;
        pub(crate) use task_event::TaskEvent;
        pub(crate) use task_wait::{
            TaskWaitKey, TaskWaitTarget, WaitResult, install_wait_target_factory,
        };
        pub(crate) use wait_completion::WaitCompletion;
    }
}
