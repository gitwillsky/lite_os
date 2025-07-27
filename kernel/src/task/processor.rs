/// Multi-core task processor implementation
///
/// This module provides the core task execution and scheduling logic for SMP systems.
/// Each CPU runs its own scheduler loop and coordinates with other CPUs for load balancing.
use alloc::{
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};
use core::sync::atomic::{AtomicU64, Ordering};

use crate::{
    arch::sbi::shutdown,
    smp::{cpu_count, cpu_data, current_cpu_data, current_cpu_id, ipi},
    sync::{SpinLock, memory_barrier},
    task::{
        __switch, add_task,
        context::TaskContext,
        task::{TaskControlBlock, TaskStatus},
        task_manager::{self, SchedulingPolicy, get_scheduling_policy},
    },
    timer::get_time_us,
    trap::TrapContext,
};

/// Global load balancer for coordinating task distribution across CPUs
static LOAD_BALANCER: SpinLock<LoadBalancer> = SpinLock::new(LoadBalancer::new());

/// Load balancing algorithm implementation
#[derive(Debug)]
struct LoadBalancer {
    /// Last load balancing timestamp
    last_balance_time: AtomicU64,
    /// Load balancing interval in microseconds
    balance_interval_us: u64,
    /// Work stealing attempts per balance cycle
    steal_attempts: usize,
}

impl LoadBalancer {
    const fn new() -> Self {
        Self {
            last_balance_time: AtomicU64::new(0),
            balance_interval_us: 100_000, // 100ms
            steal_attempts: 3,
        }
    }

    /// Check if load balancing is needed
    fn should_balance(&self) -> bool {
        let current_time = get_time_us();
        let last_balance = self.last_balance_time.load(Ordering::Relaxed);
        current_time.saturating_sub(last_balance) >= self.balance_interval_us
    }

    /// Perform load balancing across all CPUs
    fn balance_load(&self) {
        let current_time = get_time_us();
        if self
            .last_balance_time
            .compare_exchange(
                self.last_balance_time.load(Ordering::Relaxed),
                current_time,
                Ordering::AcqRel,
                Ordering::Relaxed,
            )
            .is_err()
        {
            // Another CPU is already balancing
            return;
        }

        // Collect load information from all CPUs
        let mut cpu_loads = Vec::new();
        for cpu_id in 0..cpu_count() {
            if let Some(cpu_data) = cpu_data(cpu_id) {
                let load = cpu_data.load();
                cpu_loads.push((cpu_id, load));
            }
        }

        // Sort by load (highest first)
        cpu_loads.sort_by(|a, b| b.1.cmp(&a.1));

        // Balance load from highest to lowest
        let avg_load = cpu_loads.iter().map(|(_, load)| load).sum::<usize>() / cpu_loads.len();

        for &(overloaded_cpu, load) in cpu_loads.iter() {
            if load <= avg_load + 1 {
                break; // No more overloaded CPUs
            }

            // Find underloaded CPU
            if let Some(&(underloaded_cpu, _)) = cpu_loads
                .iter()
                .find(|(_, l)| *l < avg_load.saturating_sub(1))
            {
                // Move tasks from overloaded to underloaded CPU
                if let Some(overloaded_data) = cpu_data(overloaded_cpu) {
                    let tasks_to_move = (load - avg_load) / 2; // Move half the excess
                    let stolen_tasks = overloaded_data.steal_tasks(tasks_to_move);

                    for task in stolen_tasks {
                        // Add to underloaded CPU
                        if let Some(underloaded_data) = cpu_data(underloaded_cpu) {
                            underloaded_data.add_task(task);
                        }
                    }

                    // Send reschedule IPI to underloaded CPU
                    let _ = ipi::send_reschedule_ipi(underloaded_cpu);
                }
            }
        }
    }
}

/// Statistics tracking for debugging and monitoring
static DEBUG_STATS: SpinLock<DebugStats> = SpinLock::new(DebugStats::new());

#[derive(Debug)]
struct DebugStats {
    last_debug_time: AtomicU64,
    debug_interval_us: u64,
}

impl DebugStats {
    const fn new() -> Self {
        Self {
            last_debug_time: AtomicU64::new(0),
            debug_interval_us: 5_000_000, // 5 seconds
        }
    }

    fn should_print_debug(&self) -> bool {
        let current_time = get_time_us();
        let last_time = self.last_debug_time.load(Ordering::Relaxed);

        if current_time.saturating_sub(last_time) >= self.debug_interval_us {
            self.last_debug_time.store(current_time, Ordering::Relaxed);
            true
        } else {
            false
        }
    }
}

/// Get the current task running on this CPU
pub fn current_task() -> Option<Arc<TaskControlBlock>> {
    current_cpu_data()?.current_task()
}

/// Take (remove) the current task from this CPU
pub fn take_current_task() -> Option<Arc<TaskControlBlock>> {
    if let Some(cpu_data) = current_cpu_data() {
        let task = cpu_data.current_task();
        cpu_data.set_current_task(None);
        task
    } else {
        None
    }
}

/// Get the current task's user space page table token
pub fn current_user_token() -> usize {
    current_task()
        .expect("No current task when getting user token")
        .mm
        .memory_set
        .lock()
        .token()
}

/// Get the current task's trap context
pub fn current_trap_context() -> &'static mut TrapContext {
    current_task()
        .expect("No current task when getting trap context")
        .mm
        .trap_context()
}

/// Get the current working directory
pub fn current_cwd() -> String {
    current_task()
        .map(|task| task.cwd.lock().clone())
        .unwrap_or_else(|| "/".to_string())
}

/// Main scheduler loop for all CPUs
///
/// This function runs on all CPUs and implements the per-CPU scheduler logic.
/// It handles task execution, load balancing, and idle management.
pub fn run_tasks() -> ! {
    info!("Entering scheduler main loop");

    loop {
        // Periodic maintenance
        perform_periodic_maintenance();

        // Try to get a task from the local queue first
        if let Some(task) = get_next_local_task() {
            // 直接在调度循环中进行任务切换，恢复工作版本的逻辑
            schedule_task(task);
            continue;
        }

        // No local task, try work stealing
        if let Some(stolen_task) = try_work_stealing() {
            debug!("8");
            // 直接在调度循环中进行任务切换，恢复工作版本的逻辑
            schedule_task(stolen_task);
            continue;
        }

        // No work available anywhere, enter idle state
        enter_idle_state();
        debug!("9");
    }
}

/// Perform periodic maintenance tasks
fn perform_periodic_maintenance() {
    let cpu_id = current_cpu_id();

    // Feed watchdog to indicate this CPU is alive
    if let Err(_) = crate::watchdog::feed() {
        // Watchdog may be disabled, which is fine
    }

    // Update load statistics
    if let Some(cpu_data) = current_cpu_data() {
        cpu_data.update_load_stats();
    }

    // Perform load balancing (only one CPU does this per interval)
    if cpu_id == 0 {
        // Primary CPU handles load balancing
        let load_balancer = LOAD_BALANCER.lock();
        if load_balancer.should_balance() {
            drop(load_balancer);
            LOAD_BALANCER.lock().balance_load();
        }
    }

    // Print debug information periodically
    if DEBUG_STATS.lock().should_print_debug() && cpu_id == 0 {
        print_system_debug_info();
    }
}

/// Get the next task from the local CPU queue
fn get_next_local_task() -> Option<Arc<TaskControlBlock>> {
    let cpu_data = current_cpu_data()?;
    cpu_data.pop_task()
}

/// Attempt to steal work from other CPUs
fn try_work_stealing() -> Option<Arc<TaskControlBlock>> {
    let current_cpu = current_cpu_id();
    let total_cpus = cpu_count();

    // Try to steal from each CPU in round-robin fashion
    for i in 1..total_cpus {
        let victim_cpu = (current_cpu + i) % total_cpus;

        if let Some(victim_data) = cpu_data(victim_cpu) {
            // Only steal if victim has more than one task (to avoid ping-ponging)
            if victim_data.queue_length() > 1 {
                let stolen_tasks = victim_data.steal_tasks(1);
                if let Some(task) = stolen_tasks.into_iter().next() {
                    debug!("Stole task {} from CPU{}", task.pid(), victim_cpu);
                    return Some(task);
                }
            }
        }
    }

    None
}

/// Schedule and execute a task on the current CPU (按工作版本逻辑重构)
fn schedule_task(task: Arc<TaskControlBlock>) {
    let cpu_data = match current_cpu_data() {
        Some(data) => data,
        None => {
            error!("No CPU data available for task execution");
            return;
        }
    };

    // Check if task should handle signals before execution
    if task.signal_state.lock().has_deliverable_signals() {
        handle_task_signals(&task);
        // If task was terminated by signal, don't execute it
        if task.is_zombie() {
            return;
        }
    }

    // 按照工作版本的逻辑：先设置任务状态和时间，再获取上下文指针
    *task.task_status.lock() = TaskStatus::Running;
    let start_time = get_time_us();
    task.last_runtime.store(start_time, Ordering::Relaxed);
    cpu_data
        .task_start_time
        .store(start_time, Ordering::Relaxed);

    let task_cx_ptr = {
        let task_context = task.mm.task_cx.lock();
        &*task_context as *const TaskContext
    };

    cpu_data.set_current_task(Some(task.clone()));
    let idle_cx_ptr = {
        let mut idle_context = cpu_data.idle_context.lock();
        &mut *idle_context as *mut TaskContext
    };

    // 释放 cpu_data 的锁定，避免死锁
    drop(cpu_data);

    // Switch to the task (从 idle 切换到 task，完全按照工作版本逻辑)
    unsafe {
        __switch(idle_cx_ptr, task_cx_ptr);
    }

    // 任务执行完毕，返回到调度循环，记录统计信息
    let end_time = get_time_us();
    let runtime = end_time.saturating_sub(start_time);
    task.sched.lock().update_vruntime(runtime);

    if let Some(cpu_data) = current_cpu_data() {
        cpu_data.record_task_execution(runtime, 0);
    }
}

/// Schedule function - switch to idle control flow (从工作版本移植)
fn schedule(switched_task_cx_ptr: *mut TaskContext) {
    let idle_task_cx_ptr = {
        if let Some(cpu_data) = current_cpu_data() {
            let mut idle_context = cpu_data.idle_context.lock();
            &mut *idle_context as *mut TaskContext
        } else {
            error!("No CPU data available for scheduling");
            return;
        }
    };

    unsafe {
        __switch(switched_task_cx_ptr, idle_task_cx_ptr);
    }
}

/// Handle task signals safely
fn handle_task_signals(task: &Arc<TaskControlBlock>) {
    use crate::task::signal::SignalDelivery;

    let (should_continue, exit_code) = SignalDelivery::handle_signals_safe(task);

    if !should_continue {
        if let Some(code) = exit_code {
            debug!(
                "Task {} terminated by signal with exit code {}",
                task.pid(),
                code
            );
            *task.task_status.lock() = TaskStatus::Zombie;
            task.set_exit_code(code);
        }
    }
}

/// Enter idle state on the current CPU
fn enter_idle_state() {
    let cpu_data = match current_cpu_data() {
        Some(data) => data,
        None => {
            error!("No CPU data available for idle");
            loop {
                #[cfg(target_arch = "riscv64")]
                unsafe {
                    riscv::asm::wfi();
                }

                #[cfg(not(target_arch = "riscv64"))]
                core::hint::spin_loop();
            }
        }
    };

    cpu_data.set_state(crate::smp::cpu::CpuState::Idle);
    let idle_start = get_time_us();

    // Wait for interrupt (work or timer)
    #[cfg(target_arch = "riscv64")]
    unsafe {
        riscv::asm::wfi();
    }

    #[cfg(not(target_arch = "riscv64"))]
    core::hint::spin_loop();

    let idle_end = get_time_us();
    let idle_time = idle_end.saturating_sub(idle_start);
    cpu_data.record_idle_time(idle_time);
    cpu_data.set_state(crate::smp::cpu::CpuState::Online);
}

/// Suspend the current task and run the next task (按工作版本逻辑重构)
pub fn suspend_current_and_run_next() {
    let task = take_current_task().expect("No current task to suspend");
    let end_time = get_time_us();
    let runtime = end_time.saturating_sub(task.last_runtime.load(Ordering::Relaxed));

    // Update task statistics
    task.sched.lock().update_vruntime(runtime);

    let (task_cx_ptr, should_readd) = {
        let mut task_cx = task.mm.task_cx.lock();
        let task_cx_ptr = &mut *task_cx as *mut TaskContext;
        let should_readd = *task.task_status.lock() == TaskStatus::Running;
        (task_cx_ptr, should_readd)
    };

    // If task should continue running, add it back to the queue
    if should_readd {
        *task.task_status.lock() = TaskStatus::Ready;
        if let Some(cpu_data) = current_cpu_data() {
            cpu_data.add_task(task);
        }
    }

    // 按照工作版本逻辑：直接调用 schedule 函数切换到 idle
    schedule(task_cx_ptr);
}

/// Block the current task and run the next task (按工作版本逻辑重构)
pub fn block_current_and_run_next() {
    let task = take_current_task().expect("No current task to block");
    let end_time = get_time_us();
    let runtime = end_time.saturating_sub(task.last_runtime.load(Ordering::Relaxed));

    // Update task statistics
    task.sched.lock().update_vruntime(runtime);
    let task_cx_ptr = &mut *task.mm.task_cx.lock() as *mut TaskContext;

    // Task is blocked, don't add back to queue
    // 按照工作版本逻辑：直接调用 schedule 函数切换到 idle
    schedule(task_cx_ptr);
}

/// Exit the current task and run the next task (按工作版本逻辑重构)
pub fn exit_current_and_run_next(exit_code: i32) {
    let task = take_current_task().expect("No current task to exit");
    let pid = task.pid();

    // Handle task exit
    task.set_exit_code(exit_code);
    *task.task_status.lock() = TaskStatus::Zombie;

    // Close all file descriptors and cleanup locks
    task.file.lock().close_all_fds_and_cleanup_locks(pid);

    // Reparent children to init process
    if let Some(init_proc) = task_manager::init_proc() {
        if pid == init_proc.pid() {
            error!("init process exit with exit_code {}", exit_code);
            // System should shutdown
            shutdown();
        } else {
            // Collect children to reparent
            let children_to_reparent: Vec<_> = task
                .children
                .lock()
                .iter()
                .filter(|child| child.pid() != pid)
                .cloned()
                .collect();

            if !children_to_reparent.is_empty() {
                // Reparent children
                for child in &children_to_reparent {
                    child.set_parent(Arc::downgrade(&init_proc));
                }

                // Add to init's children list
                let mut init_children = init_proc.children.lock();
                for child in children_to_reparent {
                    init_children.push(child);
                }
            }
        }
    }

    // Wake up waiting parent
    if let Some(parent) = task.parent() {
        if *parent.task_status.lock() == TaskStatus::Sleeping {
            parent.wakeup();
        }
    }

    // 按照工作版本逻辑：直接调用 schedule 函数切换到 idle
    let task_cx_ptr = &mut *task.mm.task_cx.lock() as *mut TaskContext;
    schedule(task_cx_ptr);
}

/// Mark process entry into kernel mode
pub fn mark_kernel_entry() {
    if let Some(task) = current_task() {
        let current_time = get_time_us();
        let mut in_kernel = task.in_kernel_mode.lock();

        if !*in_kernel {
            let last_runtime = task.last_runtime.load(Ordering::Relaxed);
            if current_time > last_runtime {
                let user_time = current_time - last_runtime;
                task.user_cpu_time.fetch_add(user_time, Ordering::Relaxed);
                task.total_cpu_time.fetch_add(user_time, Ordering::Relaxed);
            }

            task.kernel_enter_time
                .store(current_time, Ordering::Relaxed);
            *in_kernel = true;
        }
    }
}

/// Mark process exit from kernel mode
pub fn mark_kernel_exit() {
    if let Some(task) = current_task() {
        let current_time = get_time_us();
        let mut in_kernel = task.in_kernel_mode.lock();

        if *in_kernel {
            let kernel_enter_time = task.kernel_enter_time.load(Ordering::Relaxed);
            if current_time > kernel_enter_time {
                let kernel_time = current_time - kernel_enter_time;
                task.kernel_cpu_time
                    .fetch_add(kernel_time, Ordering::Relaxed);
                task.total_cpu_time
                    .fetch_add(kernel_time, Ordering::Relaxed);
            }

            task.last_runtime.store(current_time, Ordering::Relaxed);
            *in_kernel = false;
        }
    }
}

/// Print system debug information
fn print_system_debug_info() {
    let mut total_tasks = 0;
    let mut cpu_loads = Vec::new();

    for cpu_id in 0..cpu_count() {
        if let Some(cpu_data) = cpu_data(cpu_id) {
            let load = cpu_data.load();
            total_tasks += load;
            cpu_loads.push(load);
        }
    }

    debug!(
        "[SCHED] System status: {} total tasks, CPU loads: {:?}, time: {}μs",
        total_tasks,
        cpu_loads,
        get_time_us()
    );
}
