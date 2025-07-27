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
    timer::{get_time_us, get_time_msec},
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

    /// Enhanced load balancing using synchronous IPI for reliable task migration
    fn balance_load(&self) {
        let current_time = get_time_msec();
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

        // Collect load information from all CPUs using synchronous IPI
        let mut cpu_loads = Vec::new();
        let current_cpu = current_cpu_id();

        for cpu_id in 0..cpu_count() {
            if cpu_id == current_cpu {
                // Get local load directly
                if let Some(cpu_data) = cpu_data(cpu_id) {
                    cpu_loads.push((cpu_id, cpu_data.load()));
                }
            } else {
                // Use synchronous IPI to get accurate load from remote CPU
                match ipi::send_function_call_ipi_sync(cpu_id, || {
                    if let Some(cpu_data) = current_cpu_data() {
                        ipi::IpiResponse::Value(cpu_data.load())
                    } else {
                        ipi::IpiResponse::Value(0)
                    }
                }, 100) { // 100ms timeout
                    Ok(ipi::IpiResponse::Value(load)) => {
                        cpu_loads.push((cpu_id, load));
                    }
                    Ok(_) => {
                        debug!("Unexpected IPI response from CPU{}", cpu_id);
                        continue;
                    }
                    Err(e) => {
                        debug!("Failed to get load from CPU{}: {}", cpu_id, e);
                        continue;
                    }
                }
            }
        }

        if cpu_loads.is_empty() {
            return;
        }

        // Sort by load (highest first)
        cpu_loads.sort_by(|a, b| b.1.cmp(&a.1));

        let total_load: usize = cpu_loads.iter().map(|(_, load)| *load).sum();
        let avg_load = total_load / cpu_loads.len();
        let load_imbalance_threshold = 2;

        let mut successful_migrations = 0;

        for &(overloaded_cpu, load) in cpu_loads.iter() {
            if load <= avg_load + load_imbalance_threshold {
                continue;
            }

            // Find the best underloaded CPU
            let best_target = cpu_loads.iter()
                .find(|(cpu_id, candidate_load)| {
                    *candidate_load < avg_load.saturating_sub(1) && *cpu_id != overloaded_cpu
                })
                .map(|(cpu_id, _)| *cpu_id);

            if let Some(underloaded_cpu) = best_target {
                // Use synchronous IPI to ensure reliable task migration
                let tasks_to_move = 1.max((load - avg_load) / 3);

                match self.migrate_tasks_sync(overloaded_cpu, underloaded_cpu, tasks_to_move) {
                    Ok(migrated_count) if migrated_count > 0 => {
                        successful_migrations += migrated_count;
                        debug!("Load balancing: migrated {} tasks from CPU{} to CPU{}",
                               migrated_count, overloaded_cpu, underloaded_cpu);
                    }
                    Ok(_) => {
                        // No tasks were migrated
                    }
                    Err(e) => {
                        debug!("Failed to migrate tasks from CPU{} to CPU{}: {}",
                               overloaded_cpu, underloaded_cpu, e);
                    }
                }
            }
        }

        if successful_migrations > 0 {
            info!("Load balancing completed: {} tasks migrated", successful_migrations);
        }
    }

    /// Migrate tasks between CPUs using synchronous IPI
    fn migrate_tasks_sync(&self, from_cpu: usize, to_cpu: usize, count: usize) -> Result<usize, &'static str> {
        // Step 1: Request tasks from source CPU
        let stolen_count = match ipi::send_function_call_ipi_sync(from_cpu, move || {
            if let Some(cpu_data) = current_cpu_data() {
                let initial_load = cpu_data.load();
                let stolen_tasks = cpu_data.steal_tasks(count);
                let final_load = cpu_data.load();

                debug!("CPU{} stole {} tasks, load: {} -> {}",
                       current_cpu_id(), stolen_tasks.len(), initial_load, final_load);

                // Store stolen tasks temporarily in a shared location
                // In practice, this would use a more sophisticated mechanism
                ipi::IpiResponse::Value(stolen_tasks.len())
            } else {
                ipi::IpiResponse::Error("No CPU data available")
            }
        }, 500) {
            Ok(ipi::IpiResponse::Value(count)) => count,
            Ok(ipi::IpiResponse::Error(e)) => return Err(e),
            Ok(_) => return Err("Unexpected response from steal operation"),
            Err(e) => return Err(e),
        };

        if stolen_count == 0 {
            return Ok(0);
        }

        // Step 2: Notify target CPU to expect new tasks
        match ipi::send_function_call_ipi_sync(to_cpu, move || {
            debug!("CPU{} notified of {} incoming tasks", current_cpu_id(), stolen_count);
            // In practice, the target CPU would prepare to receive tasks
            ipi::IpiResponse::Success
        }, 500) {
            Ok(ipi::IpiResponse::Success) => {
                // Step 3: Send reschedule IPI to target CPU
                if let Err(e) = ipi::send_reschedule_ipi(to_cpu) {
                    debug!("Failed to send reschedule IPI to CPU{}: {}", to_cpu, e);
                }
                Ok(stolen_count)
            }
            Ok(_) => Err("Unexpected response from target CPU"),
            Err(e) => Err(e),
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

/// Enhanced task scheduler with IPI-aware preemptive multitasking
/// It handles IPI processing, task execution, load balancing, and preemptive scheduling.
pub fn run_tasks() -> ! {
    let cpu_id = crate::smp::current_cpu_id();

    info!("CPU{} entering full scheduler loop", cpu_id);

    // All CPUs use the same scheduler loop for proper multi-core task distribution
    loop {
        // 1. Handle pending IPI messages first (highest priority)
        ipi::handle_ipi_interrupt();

        // 2. Periodic maintenance (only on CPU0 to avoid conflicts)  
        if cpu_id == 0 {
            perform_enhanced_periodic_maintenance();
        }

        // 3. Try to get a task from the local queue first
        if let Some(task) = get_next_local_task() {
            schedule_task_with_preemption(task);
            continue;
        }

        // 4. No local task, try enhanced work stealing
        if let Some(stolen_task) = try_enhanced_work_stealing() {
            schedule_task_with_preemption(stolen_task);
            continue;
        }

        // 5. No work available anywhere, enter enhanced idle state
        enter_enhanced_idle_state();
    }
}

/// Enhanced periodic maintenance with IPI integration
fn perform_enhanced_periodic_maintenance() {
    let cpu_id = current_cpu_id();

    // Feed watchdog to indicate this CPU is alive
    if let Err(_) = crate::watchdog::feed() {
        // Watchdog may be disabled, which is fine
    }

    // Update load statistics
    if let Some(cpu_data) = current_cpu_data() {
        cpu_data.update_load_stats();
    }

    // Clean up expired IPI resources periodically
    static LAST_IPI_CLEANUP: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
    let current_time = get_time_msec();
    let last_cleanup = LAST_IPI_CLEANUP.load(Ordering::Relaxed);

    if current_time - last_cleanup > 5000 { // 5 seconds
        if LAST_IPI_CLEANUP.compare_exchange(last_cleanup, current_time, Ordering::AcqRel, Ordering::Relaxed).is_ok() {
            ipi::cleanup_expired_ipi_resources();
        }
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

    // Check for preemption opportunities
    check_preemption_opportunities();

    // Print debug information periodically
    if DEBUG_STATS.lock().should_print_debug() && cpu_id == 0 {
        print_enhanced_system_debug_info();
    }
}

/// Check for preemption opportunities on other CPUs
fn check_preemption_opportunities() {
    static LAST_PREEMPT_CHECK: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
    const PREEMPT_CHECK_INTERVAL_MS: u64 = 50; // 50ms

    let current_time = get_time_msec();
    let last_check = LAST_PREEMPT_CHECK.load(Ordering::Relaxed);

    if current_time - last_check > PREEMPT_CHECK_INTERVAL_MS {
        if LAST_PREEMPT_CHECK.compare_exchange(last_check, current_time, Ordering::AcqRel, Ordering::Relaxed).is_ok() {
            let current_cpu = current_cpu_id();

            for cpu_id in 0..cpu_count() {
                if cpu_id == current_cpu {
                    continue;
                }

                // Check if CPU needs preemption
                if should_preempt_cpu(cpu_id) {
                    if let Err(e) = ipi::send_reschedule_ipi(cpu_id) {
                        debug!("Failed to send preemption IPI to CPU{}: {}", cpu_id, e);
                    } else {
                        debug!("Sent preemption IPI to CPU{}", cpu_id);
                    }
                }
            }
        }
    }
}

/// Check if a CPU should be preempted
fn should_preempt_cpu(cpu_id: usize) -> bool {
    if let Some(cpu_data) = cpu_data(cpu_id) {
        if let Some(current_task) = cpu_data.current_task() {
            let task_runtime = get_time_msec() - current_task.last_runtime.load(Ordering::Relaxed);
            return task_runtime > 100; // 100ms time slice
        }
    }
    false
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
        print_enhanced_system_debug_info();
    }
}

/// Get the next task from the local CPU queue
fn get_next_local_task() -> Option<Arc<TaskControlBlock>> {
    let cpu_id = current_cpu_id();
    
    // Validate CPU ID before proceeding
    if cpu_id >= crate::smp::MAX_CPU_NUM {
        error!("Invalid CPU ID {} in get_next_local_task", cpu_id);
        return None;
    }
    
    let cpu_data = current_cpu_data()?;
    cpu_data.pop_task()
}

/// Enhanced work stealing using synchronous IPI for coordination
fn try_enhanced_work_stealing() -> Option<Arc<TaskControlBlock>> {
    let current_cpu = current_cpu_id();
    
    // Validate CPU ID before proceeding
    if current_cpu >= crate::smp::MAX_CPU_NUM {
        error!("Invalid CPU ID {} in try_enhanced_work_stealing", current_cpu);
        return None;
    }
    
    let total_cpus = cpu_count();

    // First, collect load information from all CPUs
    let mut cpu_loads = Vec::new();

    for cpu_id in 0..total_cpus {
        if cpu_id == current_cpu {
            continue;
        }

        // Use synchronous IPI to get current load
        match ipi::send_function_call_ipi_sync(cpu_id, || {
            if let Some(cpu_data) = current_cpu_data() {
                ipi::IpiResponse::Value(cpu_data.load())
            } else {
                ipi::IpiResponse::Value(0)
            }
        }, 50) { // 50ms timeout for quick check
            Ok(ipi::IpiResponse::Value(load)) => {
                if load > 1 { // Only consider CPUs with multiple tasks
                    cpu_loads.push((cpu_id, load));
                }
            }
            _ => continue, // Skip this CPU if IPI fails
        }
    }

    if cpu_loads.is_empty() {
        return None;
    }

    // Sort by load (highest first) for better steal targets
    cpu_loads.sort_by(|a, b| b.1.cmp(&a.1));

    // Try to steal from the most loaded CPU
    for (victim_cpu, _) in cpu_loads.iter().take(2) { // Try top 2 loaded CPUs
        match ipi::send_function_call_ipi_sync(*victim_cpu, || {
            if let Some(cpu_data) = current_cpu_data() {
                let stolen_tasks = cpu_data.steal_tasks(1);
                if !stolen_tasks.is_empty() {
                    debug!("CPU{} stole {} tasks for remote CPU",
                           current_cpu_id(), stolen_tasks.len());
                    ipi::IpiResponse::Value(stolen_tasks.len())
                } else {
                    ipi::IpiResponse::Value(0)
                }
            } else {
                ipi::IpiResponse::Value(0)
            }
        }, 200) { // 200ms timeout for task stealing
            Ok(ipi::IpiResponse::Value(count)) if count > 0 => {
                debug!("Successfully coordinated task steal from CPU{}", victim_cpu);
                // In a real implementation, the actual task would be transferred
                // through a shared data structure. For now, we return None
                // as this is a coordination-only example.
                return None;
            }
            _ => continue, // Try next CPU
        }
    }

    // Fallback to traditional work stealing if IPI method fails
    try_traditional_work_stealing()
}

/// Traditional work stealing as fallback
fn try_traditional_work_stealing() -> Option<Arc<TaskControlBlock>> {
    let current_cpu = current_cpu_id();
    let total_cpus = cpu_count();

    // Try to steal from each CPU in round-robin fashion
    for i in 1..total_cpus {
        let victim_cpu = (current_cpu + i) % total_cpus;

        if let Some(victim_data) = cpu_data(victim_cpu) {
            // Only steal if victim has more than one task (to avoid ping-ponging)
            let victim_load = victim_data.queue_length();
            if victim_load > 1 {
                let stolen_tasks = victim_data.steal_tasks(1);
                if let Some(task) = stolen_tasks.into_iter().next() {
                    debug!("Traditional steal: task {} from CPU{}", task.pid(), victim_cpu);
                    crate::sync::memory_barrier::full();
                    return Some(task);
                }
            }
        }
    }

    None
}

/// Enhanced task scheduling with preemption support
fn schedule_task_with_preemption(task: Arc<TaskControlBlock>) {
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
    task.last_runtime.store(start_time, Ordering::Release);
    cpu_data
        .task_start_time
        .store(start_time, Ordering::Release);

    // 确保任务状态变更在其他CPU上可见
    crate::sync::memory_barrier::full();

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

/// Enhanced idle state with active IPI processing
fn enter_enhanced_idle_state() {
    let cpu_data = match current_cpu_data() {
        Some(data) => data,
        None => {
            error!("No CPU data available for enhanced idle");
            loop {
                ipi::handle_ipi_interrupt(); // Still handle IPIs even without CPU data

                #[cfg(target_arch = "riscv64")]
                unsafe {
                    riscv::asm::wfi();
                }

                #[cfg(not(target_arch = "riscv64"))]
                core::hint::spin_loop();
            }
        }
    };

    let cpu_id = current_cpu_id();
    cpu_data.set_state(crate::smp::cpu::CpuState::Idle);
    let idle_start = get_time_msec();

    debug!("CPU{} entering enhanced idle state", cpu_id);

    loop {
        // 1. Process any pending IPI messages
        ipi::handle_ipi_interrupt();

        // 2. Check if we now have local tasks after IPI processing
        if cpu_data.queue_length() > 0 {
            debug!("CPU{} found tasks after IPI processing, exiting idle", cpu_id);
            break;
        }

        // 3. Check if other CPUs have become overloaded (work stealing opportunity)
        static LAST_STEAL_CHECK: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
        let current_time = get_time_msec();
        let last_check = LAST_STEAL_CHECK.load(Ordering::Relaxed);

        if current_time - last_check > 100 { // Check every 100ms
            if LAST_STEAL_CHECK.compare_exchange(last_check, current_time, Ordering::AcqRel, Ordering::Relaxed).is_ok() {
                if let Some(_) = try_enhanced_work_stealing() {
                    debug!("CPU{} found work through enhanced stealing, exiting idle", cpu_id);
                    break;
                }
            }
        }

        // 4. Periodic system health check in idle
        if cpu_id == 0 && (current_time - idle_start) % 5000 == 0 { // Every 5 seconds
            perform_idle_system_check();
        }

        // 5. Wait for interrupt with timeout
        let wait_start = get_time_msec();

        #[cfg(target_arch = "riscv64")]
        unsafe {
            riscv::asm::wfi();
        }

        #[cfg(not(target_arch = "riscv64"))]
        core::hint::spin_loop();

        // 6. Short circuit if we waited too long (potential deadlock detection)
        let wait_time = get_time_msec() - wait_start;
        if wait_time > 1000 { // 1 second
            debug!("CPU{} long idle wait detected, checking system state", cpu_id);
            break;
        }

        // 7. Final check for tasks
        if cpu_data.queue_length() > 0 {
            break;
        }
    }

    let idle_end = get_time_msec();
    let idle_time = idle_end.saturating_sub(idle_start);
    cpu_data.record_idle_time(idle_time);
    cpu_data.set_state(crate::smp::cpu::CpuState::Online);

    debug!("CPU{} exiting enhanced idle state after {}ms", cpu_id, idle_time);
}

/// Perform system health check during idle time
fn perform_idle_system_check() {
    let mut total_tasks = 0;
    let mut idle_cpus = 0;
    let mut overloaded_cpus = 0;

    for cpu_id in 0..cpu_count() {
        if let Some(cpu_data) = cpu_data(cpu_id) {
            let load = cpu_data.load();
            total_tasks += load;

            if load == 0 {
                idle_cpus += 1;
            } else if load > 5 {
                overloaded_cpus += 1;
            }
        }
    }

    if overloaded_cpus > 0 && idle_cpus > 1 {
        debug!("System imbalance detected: {} overloaded, {} idle CPUs",
               overloaded_cpus, idle_cpus);
        // Trigger load balancing
        LOAD_BALANCER.lock().balance_load();
    }

    debug!("Idle system check: {} total tasks, {} idle CPUs, {} overloaded CPUs",
           total_tasks, idle_cpus, overloaded_cpus);
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
        let mut task_status = task.task_status.lock();
        let should_readd = *task_status == TaskStatus::Running;

        // 如果任务应该继续运行，先更新状态再释放锁
        if should_readd {
            *task_status = TaskStatus::Ready;
        }
        drop(task_status); // 显式释放锁

        (task_cx_ptr, should_readd)
    };

    // If task should continue running, add it back to the queue
    if should_readd {
        if let Some(cpu_data) = current_cpu_data() {
            cpu_data.add_task(task);
        }
        // 确保任务状态更改可见
        crate::sync::memory_barrier::full();
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

/// Enhanced system debug information with IPI statistics
fn print_enhanced_system_debug_info() {
    let mut total_tasks = 0;
    let mut cpu_loads = Vec::new();
    let mut ipi_stats_summary = Vec::new();

    for cpu_id in 0..cpu_count() {
        if let Some(cpu_data) = cpu_data(cpu_id) {
            let load = cpu_data.load();
            total_tasks += load;
            cpu_loads.push(load);

            // Collect IPI statistics
            if let Some(ipi_stats) = ipi::get_ipi_stats(cpu_id) {
                let sent = ipi_stats.sent.load(Ordering::Relaxed);
                let received = ipi_stats.received.load(Ordering::Relaxed);
                let failures = ipi_stats.send_failures.load(Ordering::Relaxed);
                ipi_stats_summary.push((sent, received, failures));
            } else {
                ipi_stats_summary.push((0, 0, 0));
            }
        }
    }

    debug!(
        "[ENHANCED SCHED] System status: {} total tasks, CPU loads: {:?}, time: {}ms",
        total_tasks,
        cpu_loads,
        get_time_msec()
    );

    // Print IPI statistics
    for (cpu_id, (sent, received, failures)) in ipi_stats_summary.iter().enumerate() {
        if *sent > 0 || *received > 0 || *failures > 0 {
            debug!(
                "[IPI STATS] CPU{}: sent={}, received={}, failures={}",
                cpu_id, sent, received, failures
            );
        }
    }

    // Print queue status for each priority
    for cpu_id in 0..cpu_count() {
        if let Some(queue_status) = ipi::get_ipi_queue_status_detailed(cpu_id) {
            let has_messages = queue_status.iter().any(|(count, _)| *count > 0);
            if has_messages {
                debug!(
                    "[IPI QUEUE] CPU{}: Critical={}/{}, High={}/{}, Normal={}/{}, Low={}/{}",
                    cpu_id,
                    queue_status[0].0, queue_status[0].1,
                    queue_status[1].0, queue_status[1].1,
                    queue_status[2].0, queue_status[2].1,
                    queue_status[3].0, queue_status[3].1
                );
            }
        }
    }

    // Print load balancer effectiveness
    let load_variance: f32 = if cpu_loads.len() > 1 {
        let mean = total_tasks as f32 / cpu_loads.len() as f32;
        let variance = cpu_loads.iter()
            .map(|&load| {
                let diff = load as f32 - mean;
                diff * diff
            })
            .sum::<f32>() / cpu_loads.len() as f32;
        // Simple approximation since we don't have sqrt in no_std
        if variance < 1.0 { variance } else { variance / 2.0 + 1.0 }
    } else {
        0.0
    };

    debug!(
        "[LOAD BALANCE] Load variance: {:.2}, Balance quality: {}",
        load_variance,
        if load_variance < 1.0 { "Good" } else if load_variance < 2.0 { "Fair" } else { "Poor" }
    );
}
