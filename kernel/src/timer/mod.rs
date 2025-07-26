/// Multi-core timer management
///
/// This module provides timer services for SMP systems, including per-CPU
/// timers and global time synchronization.

use core::sync::atomic::{AtomicU64, Ordering};
use alloc::{collections::BTreeMap, sync::Arc, vec::Vec};

use crate::{
    smp::{current_cpu_id, cpu_data, cpu_count},
    sync::SpinLock,
    task::TaskControlBlock,
};

/// Time specification structure
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct TimeSpec {
    pub sec: i64,
    pub nsec: i64,
}

impl TimeSpec {
    pub const fn new(sec: i64, nsec: i64) -> Self {
        Self { sec, nsec }
    }

    pub fn zero() -> Self {
        Self::new(0, 0)
    }

    pub fn to_microseconds(&self) -> u64 {
        (self.sec as u64 * 1_000_000) + (self.nsec as u64 / 1000)
    }

    pub fn from_microseconds(us: u64) -> Self {
        Self {
            sec: (us / 1_000_000) as i64,
            nsec: ((us % 1_000_000) * 1000) as i64,
        }
    }
}

/// Global time synchronization
///
/// This structure manages time synchronization across all CPUs.
struct GlobalTimer {
    /// Global time base in microseconds (monotonic)
    global_time_base: AtomicU64,
    /// Boot time in microseconds
    boot_time: AtomicU64,
    /// Per-CPU time offsets for synchronization
    cpu_offsets: [AtomicU64; crate::smp::MAX_CPU_NUM],
}

impl GlobalTimer {
    const fn new() -> Self {
        const INIT_OFFSET: AtomicU64 = AtomicU64::new(0);
        Self {
            global_time_base: AtomicU64::new(0),
            boot_time: AtomicU64::new(0),
            cpu_offsets: [INIT_OFFSET; crate::smp::MAX_CPU_NUM],
        }
    }

    /// Initialize global timer
    fn init(&self) {
        let boot_time = self.get_hardware_time_us();
        self.boot_time.store(boot_time, Ordering::Relaxed);
        self.global_time_base.store(0, Ordering::Relaxed);

        info!("Global timer initialized at boot time {}μs", boot_time);
    }

    /// Get hardware time in microseconds
    fn get_hardware_time_us(&self) -> u64 {
        #[cfg(target_arch = "riscv64")]
        {
            use riscv::register::time;
            // RISC-V timer frequency is typically 10MHz
            const TIMER_FREQ: u64 = 10_000_000;
            time::read64() / (TIMER_FREQ / 1_000_000)
        }

        #[cfg(not(target_arch = "riscv64"))]
        {
            // Fallback for other architectures
            static mut FAKE_TIME: u64 = 0;
            unsafe {
                FAKE_TIME += 1000; // 1ms increment
                FAKE_TIME
            }
        }
    }

    /// Get current monotonic time in microseconds
    fn get_time_us(&self) -> u64 {
        let hardware_time = self.get_hardware_time_us();
        let boot_time = self.boot_time.load(Ordering::Relaxed);
        hardware_time.saturating_sub(boot_time)
    }

    /// Synchronize time on a CPU
    fn sync_cpu_time(&self, cpu_id: usize) {
        if cpu_id < crate::smp::MAX_CPU_NUM {
            let global_time = self.get_time_us();
            let local_time = self.get_hardware_time_us();
            let offset = global_time.saturating_sub(local_time);
            self.cpu_offsets[cpu_id].store(offset, Ordering::Relaxed);

            debug!("Synchronized CPU {} time, offset: {}μs", cpu_id, offset);
        }
    }
}

/// Global timer instance
static GLOBAL_TIMER: GlobalTimer = GlobalTimer::new();

/// Sleep queue for managing sleeping tasks
struct SleepQueue {
    /// Tasks sleeping until a specific time
    sleeping_tasks: BTreeMap<u64, Vec<Arc<TaskControlBlock>>>,
}

impl SleepQueue {
    fn new() -> Self {
        Self {
            sleeping_tasks: BTreeMap::new(),
        }
    }

    /// Add a task to sleep until the specified time
    fn add_sleeping_task(&mut self, wake_time: u64, task: Arc<TaskControlBlock>) {
        debug!("Task {} sleeping until {}μs", task.pid(), wake_time);
        self.sleeping_tasks.entry(wake_time).or_insert_with(Vec::new).push(task);
    }

    /// Wake up tasks that should wake up before or at the specified time
    fn wake_tasks_before(&mut self, current_time: u64) -> Vec<Arc<TaskControlBlock>> {
        let mut woken_tasks = Vec::new();

        // Find all tasks that should wake up
        let wake_times: Vec<u64> = self.sleeping_tasks
            .range(..=current_time)
            .map(|(&time, _)| time)
            .collect();

        // Remove and collect all tasks that should wake up
        for wake_time in wake_times {
            if let Some(tasks) = self.sleeping_tasks.remove(&wake_time) {
                for task in tasks {
                    debug!("Waking up task {} at {}μs", task.pid(), current_time);
                    woken_tasks.push(task);
                }
            }
        }

        woken_tasks
    }

    /// Get the next wake time
    fn next_wake_time(&self) -> Option<u64> {
        self.sleeping_tasks.keys().next().copied()
    }

    /// Get the number of sleeping tasks
    fn len(&self) -> usize {
        self.sleeping_tasks.values().map(|v| v.len()).sum()
    }
}

/// Global sleep queue
static SLEEP_QUEUE: SpinLock<SleepQueue> = SpinLock::new(SleepQueue {
    sleeping_tasks: BTreeMap::new(),
});

/// Initialize timer subsystem (called by primary CPU)
pub fn init() {
    info!("Initializing timer subsystem");

    GLOBAL_TIMER.init();

    // Set up timer interrupt for the primary CPU
    setup_timer_interrupt();

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

        // Set next timer interrupt (1ms from now)
        let current_time = time::read64();
        let next_time = current_time + 10_000; // 1ms at 10MHz

        // Use SBI to set timer
        crate::arch::sbi::set_timer(next_time as usize);

        debug!("Timer interrupt set up for CPU {}", current_cpu_id());
    }
}

/// Handle timer interrupt
pub fn handle_timer_interrupt() {
    let cpu_id = current_cpu_id();

    // Update CPU statistics
    if let Some(cpu_data) = cpu_data(cpu_id) {
        cpu_data.stats.interrupts_handled.fetch_add(1, Ordering::Relaxed);
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
    #[cfg(target_arch = "riscv64")]
    {
        use riscv::register::time;

        let current_time = time::read64();
        let next_time = current_time + 10_000; // 1ms at 10MHz

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

/// Get Unix timestamp in seconds
pub fn get_unix_timestamp() -> u64 {
    // For simplicity, return time since boot
    // In a real implementation, this should return actual Unix timestamp
    get_time_us() / 1_000_000
}

/// Get Unix timestamp in microseconds
pub fn get_unix_timestamp_us() -> u64 {
    // For simplicity, return time since boot
    // In a real implementation, this should return actual Unix timestamp
    get_time_us()
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

/// Get current time as TimeSpec
pub fn get_time() -> TimeSpec {
    TimeSpec::from_microseconds(get_time_us())
}

/// Sleep the current task for the specified duration
pub fn sleep_current_task(duration_us: u64) {
    if let Some(current_task) = crate::task::current_task() {
        let wake_time = get_time_us() + duration_us;

        // Set task as sleeping
        *current_task.task_status.lock() = crate::task::TaskStatus::Sleeping;

        // Add to sleep queue
        SLEEP_QUEUE.lock().add_sleeping_task(wake_time, current_task);

        // Block current task
        crate::task::block_current_and_run_next();
    }
}

/// Sleep until absolute time
pub fn sleep_until(wake_time_us: u64) {
    let current_time = get_time_us();
    if wake_time_us > current_time {
        sleep_current_task(wake_time_us - current_time);
    }
}

/// Sleep for nanoseconds
pub fn nanosleep(duration_ns: u64) {
    let duration_us = (duration_ns + 999) / 1000; // Round up to microseconds
    sleep_current_task(duration_us);
}

/// Get system uptime in microseconds
pub fn uptime_us() -> u64 {
    get_time_us()
}

/// Get system uptime as TimeSpec
pub fn uptime() -> TimeSpec {
    TimeSpec::from_microseconds(uptime_us())
}

/// Set a timer for a specific task (used for timeouts)
pub fn set_task_timer(task: Arc<TaskControlBlock>, timeout_us: u64) {
    let wake_time = get_time_us() + timeout_us;
    SLEEP_QUEUE.lock().add_sleeping_task(wake_time, task);
}

/// Cancel a timer for a specific task
pub fn cancel_task_timer(task: &Arc<TaskControlBlock>) {
    let mut sleep_queue = SLEEP_QUEUE.lock();

    // Remove the task from all wake times (inefficient but correct)
    let mut wake_times_to_remove = Vec::new();

    for (&wake_time, tasks) in sleep_queue.sleeping_tasks.iter_mut() {
        tasks.retain(|t| !Arc::ptr_eq(t, task));
        if tasks.is_empty() {
            wake_times_to_remove.push(wake_time);
        }
    }

    // Remove empty wake times
    for wake_time in wake_times_to_remove {
        sleep_queue.sleeping_tasks.remove(&wake_time);
    }
}

/// Get timer statistics
pub fn get_timer_stats() -> (usize, Option<u64>, u64) {
    let sleep_queue = SLEEP_QUEUE.lock();
    let sleeping_count = sleep_queue.len();
    let next_wake = sleep_queue.next_wake_time();
    let current_time = get_time_us();

    (sleeping_count, next_wake, current_time)
}

/// Print timer debug information
pub fn print_timer_info() {
    let (sleeping_count, next_wake, current_time) = get_timer_stats();

    info!("=== Timer Information ===");
    info!("Current time: {}μs", current_time);
    info!("Uptime: {}μs", uptime_us());
    info!("Sleeping tasks: {}", sleeping_count);

    if let Some(next_wake_time) = next_wake {
        info!("Next wake time: {}μs (in {}μs)",
              next_wake_time,
              next_wake_time.saturating_sub(current_time));
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

/// High-precision delay (busy wait)
///
/// This should only be used for very short delays where sleeping is not appropriate.
pub fn udelay(microseconds: u64) {
    let start = get_time_us();
    while get_time_us() - start < microseconds {
        core::hint::spin_loop();
    }
}

/// Convert TimeSpec to microseconds
impl TimeSpec {
    pub fn to_us(&self) -> u64 {
        self.to_microseconds()
    }
}

/// Time conversion utilities
pub mod time_utils {
    use super::*;

    pub const USEC_PER_SEC: u64 = 1_000_000;
    pub const NSEC_PER_SEC: u64 = 1_000_000_000;
    pub const NSEC_PER_USEC: u64 = 1_000;

    pub fn us_to_ns(us: u64) -> u64 {
        us * NSEC_PER_USEC
    }

    pub fn ns_to_us(ns: u64) -> u64 {
        (ns + NSEC_PER_USEC - 1) / NSEC_PER_USEC // Round up
    }

    pub fn sec_to_us(sec: u64) -> u64 {
        sec * USEC_PER_SEC
    }

    pub fn us_to_sec(us: u64) -> u64 {
        us / USEC_PER_SEC
    }
}

/// Re-export commonly used time types
pub use time_utils::*;