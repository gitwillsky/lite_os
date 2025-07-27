use alloc::{collections::BTreeMap, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicU64, Ordering};

use crate::{
    board,
    smp::{cpu_count, cpu_data, current_cpu_id},
    sync::SpinLock,
    task::TaskControlBlock,
    timer::{global_timer::GLOBAL_TIMER, rtc::init_rtc_device, sleep_queue::SLEEP_QUEUE},
};

mod config;
mod global_timer;
mod rtc;
mod sleep_queue;
mod spec;

use config::*;

use spec::TimeSpec;

pub use rtc::{get_unix_timestamp, get_unix_timestamp_us};

/// Initialize timer subsystem (called by primary CPU)
pub fn init() {
    info!("Initializing timer subsystem");

    let time_base_freq = board::board_info().time_base_freq;
    TIMER_FREQ.store(time_base_freq, Ordering::Relaxed);
    TICK_INTERVAL_VALUE.store(time_base_freq / TICKS_PER_SEC as u64, Ordering::Relaxed);

    GLOBAL_TIMER.init();

    // Set up timer interrupt for the primary CPU
    setup_timer_interrupt();

    // Initialize RTC
    init_rtc_device();

    info!("Timer subsystem initialized");
}

/// Initialize timer for a secondary CPU
pub fn init_secondary_cpu(cpu_id: usize) {
    debug!("Initializing timer for CPU {}", cpu_id);

    // Synchronize time with global timer
    GLOBAL_TIMER.sync_cpu_time(cpu_id);

    // Set up timer interrupt for this CPU
    setup_timer_interrupt();

    debug!("Timer initialized for CPU {}", cpu_id);
}

/// Set up timer interrupt for the current CPU
fn setup_timer_interrupt() {
    #[cfg(target_arch = "riscv64")]
    {
        use riscv::register::{sie, time, timeh};

        // Enable timer interrupt
        unsafe {
            sie::set_stimer();
        }

        set_next_timer_interrupt();
        debug!("Timer interrupt set up for CPU {}", current_cpu_id());
    }
}

/// Handle timer interrupt
pub fn handle_timer_interrupt() {
    let cpu_id = current_cpu_id();

    // Update CPU statistics
    if let Some(cpu_data) = cpu_data(cpu_id) {
        cpu_data
            .stats
            .interrupts_handled
            .fetch_add(1, Ordering::Relaxed);
    }

    // Process sleeping tasks (only on CPU 0 to avoid race conditions)
    if cpu_id == 0 {
        process_sleeping_tasks();
    }

    // Set next timer interrupt
    set_next_timer_interrupt();

    // Trigger rescheduling if needed
    if let Some(cpu_data) = cpu_data(cpu_id) {
        if cpu_data.need_resched() {
            crate::task::suspend_current_and_run_next();
        }
    }
}

/// Process sleeping tasks and wake them up if needed
fn process_sleeping_tasks() {
    let current_time = get_time_us();
    let woken_tasks = SLEEP_QUEUE.lock().wake_tasks_before(current_time);

    // Add woken tasks back to the scheduler
    for task in woken_tasks {
        // Set task as ready
        *task.task_status.lock() = crate::task::TaskStatus::Ready;

        // Add to task manager
        crate::task::add_task(task);
    }
}

/// Set the next timer interrupt
pub fn set_next_timer_interrupt() {
    assert_ne!(
        TICK_INTERVAL_VALUE.load(Ordering::Relaxed),
        0,
        "TICK_INTERVAL_VALUE is 0"
    );

    #[cfg(target_arch = "riscv64")]
    {
        use riscv::register::time;

        let current_time = time::read64();
        let next_time = current_time + TICK_INTERVAL_VALUE.load(Ordering::Relaxed);

        crate::arch::sbi::set_timer(next_time as usize);
    }
}

/// Get current time in microseconds since boot
pub fn get_time_us() -> u64 {
    GLOBAL_TIMER.get_time_us()
}

/// Get current time in milliseconds since boot
pub fn get_time_msec() -> u64 {
    get_time_us() / 1000
}

/// Get current time in nanoseconds since boot
pub fn get_time_ns() -> u64 {
    get_time_us() * 1000
}

/// Check and wake up sleeping tasks
pub fn check_and_wakeup_sleeping_tasks() {
    let current_time = get_time_us();
    let mut sleep_queue = SLEEP_QUEUE.lock();
    let woken_tasks = sleep_queue.wake_tasks_before(current_time);

    // Add woken tasks back to scheduler
    for task in woken_tasks {
        crate::task::add_task(task);
    }
}

/// Sleep the current task for the specified duration
fn sleep_current_task(duration_us: u64) {
    if let Some(current_task) = crate::task::current_task() {
        let wake_time = get_time_us() + duration_us;

        // Set task as sleeping
        *current_task.task_status.lock() = crate::task::TaskStatus::Sleeping;

        // Add to sleep queue
        SLEEP_QUEUE
            .lock()
            .add_sleeping_task(wake_time, current_task);

        // Block current task
        crate::task::block_current_and_run_next();
    }
}

/// Sleep for nanoseconds
pub fn nanosleep(duration_ns: u64) {
    let duration_us = (duration_ns + 999) / 1000; // Round up to microseconds
    sleep_current_task(duration_us);
}

/// Print timer debug information
pub fn print_timer_info() {
    let sleep_queue = SLEEP_QUEUE.lock();
    let sleeping_count = sleep_queue.len();
    let next_wake = sleep_queue.next_wake_time();
    let current_time = get_unix_timestamp_us();

    info!("=== Timer Information ===");
    info!("Current time: {}μs", current_time);
    info!("Uptime: {}μs", get_time_us());
    info!("Sleeping tasks: {}", sleeping_count);

    if let Some(next_wake_time) = next_wake {
        info!(
            "Next wake time: {}μs (in {}μs)",
            next_wake_time,
            next_wake_time.saturating_sub(current_time)
        );
    } else {
        info!("No tasks sleeping");
    }

    // Print per-CPU timer statistics
    info!("Per-CPU Timer Stats:");
    for cpu_id in 0..cpu_count() {
        if let Some(cpu_data) = cpu_data(cpu_id) {
            let interrupts = cpu_data.stats.interrupts_handled.load(Ordering::Relaxed);
            info!("  CPU{}: interrupts_handled={}", cpu_id, interrupts);
        }
    }

    info!("========================");
}
