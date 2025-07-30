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

    /// Simple load balancing without synchronous IPIs (safe during boot)
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

        // Collect load information from all CPUs without using sync IPIs
        let mut cpu_loads = Vec::new();

        for cpu_id in 0..cpu_count() {
            // Get load directly from CPU data (no IPI needed)
            if let Some(cpu_data) = cpu_data(cpu_id) {
                cpu_loads.push((cpu_id, cpu_data.load()));
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
                // Use simple task migration without sync IPIs
                let tasks_to_move = 1.max((load - avg_load) / 3);

                if let Some(overloaded_data) = cpu_data(overloaded_cpu) {
                    let stolen_tasks = overloaded_data.steal_tasks(tasks_to_move);
                    if !stolen_tasks.is_empty() {
                        if let Some(underloaded_data) = cpu_data(underloaded_cpu) {
                            for task in stolen_tasks {
                                underloaded_data.add_task(task);
                                successful_migrations += 1;
                            }
                            debug!("Load balancing: migrated {} tasks from CPU{} to CPU{}",
                                   successful_migrations, overloaded_cpu, underloaded_cpu);
                        }
                    }
                }
            }
        }

        if successful_migrations > 0 {
            info!("Load balancing completed: {} tasks migrated", successful_migrations);
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
    debug!("CPU{}: Starting task scheduler loop", cpu_id);

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
            debug!("CPU{}: Found local task {} to execute", cpu_id, task.pid());
            // 关键修复：检查任务状态是否可执行
            let task_status = *task.task_status.lock();
            if task_status == TaskStatus::Ready {
                debug!("CPU{}: Executing task {} (status: Ready)", cpu_id, task.pid());
                schedule_task_with_preemption(task);
                continue;
            } else {
                debug!("CPU{}: Skipping task {} with invalid status: {:?}", cpu_id, task.pid(), task_status);
                continue;
            }
        }

        // 4. No local task, try traditional work stealing (avoid sync IPI during boot)
        if let Some(stolen_task) = try_traditional_work_stealing() {
            let stolen_task_status = *stolen_task.task_status.lock();
            if stolen_task_status == TaskStatus::Ready {
                schedule_task_with_preemption(stolen_task);
                continue;
            } else {
                debug!("CPU{}: Skipping stolen task {} with invalid status: {:?}", cpu_id, stolen_task.pid(), stolen_task_status);
                continue;
            }
        }

        // 5. No work available anywhere, enter simple idle state
        debug!("CPU{}: Entering idle state (no tasks found)", cpu_id);
        enter_simple_idle_state();
    }
}

/// Simple periodic maintenance without problematic sync IPIs
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

    // Simple load balancing without sync IPIs
    if cpu_id == 0 {
        // Primary CPU handles load balancing
        let load_balancer = LOAD_BALANCER.lock();
        if load_balancer.should_balance() {
            drop(load_balancer);
            LOAD_BALANCER.lock().balance_load();
        }
    }

    // Trigger global load balancing from task manager
    static LAST_GLOBAL_BALANCE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
    let current_time = get_time_msec();
    let last_balance = LAST_GLOBAL_BALANCE.load(Ordering::Relaxed);

    if current_time - last_balance > 1000 && cpu_id == 0 { // Every 1 second, only CPU0
        if LAST_GLOBAL_BALANCE.compare_exchange(last_balance, current_time, Ordering::AcqRel, Ordering::Relaxed).is_ok() {
            crate::task::task_manager::perform_global_load_balance();
        }
    }
}


/// Get the next task from the local CPU queue or global pool
fn get_next_local_task() -> Option<Arc<TaskControlBlock>> {
    let cpu_id = current_cpu_id();

    // Validate CPU ID before proceeding
    if cpu_id >= crate::smp::MAX_CPU_NUM {
        error!("Invalid CPU ID {} in get_next_local_task", cpu_id);
        return None;
    }

    // First try local CPU queue
    if let Some(cpu_data) = current_cpu_data() {
        if let Some(task) = cpu_data.pop_task() {
            return Some(task);
        }
    }

    // If no local task, try global task manager
    if let Some(task) = crate::task::task_manager::fetch_task() {
        return Some(task);
    }

    // 关键修复：如果仍然没有任务，主动检查是否有任务被分配但未处理
    static LAST_TASK_CHECK: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
    let current_time = crate::timer::get_time_msec();
    let last_check = LAST_TASK_CHECK.load(core::sync::atomic::Ordering::Relaxed);

    if current_time.saturating_sub(last_check) > 100 { // 每100ms检查一次
        if LAST_TASK_CHECK.compare_exchange(last_check, current_time,
            core::sync::atomic::Ordering::AcqRel, core::sync::atomic::Ordering::Relaxed).is_ok() {

            // 强制触发全局负载均衡
            crate::task::task_manager::perform_global_load_balance();

            // 再次尝试获取任务
            if let Some(cpu_data) = current_cpu_data() {
                if let Some(task) = cpu_data.pop_task() {
                    debug!("CPU{}: Found task {} after load balance", cpu_id, task.pid());
                    return Some(task);
                }
            }
        }
    }

    None
}


/// Enhanced work stealing algorithm with multiple strategies
fn try_traditional_work_stealing() -> Option<Arc<TaskControlBlock>> {
    let current_cpu = current_cpu_id();
    let total_cpus = cpu_count();

    if total_cpus <= 1 {
        return None;
    }

    // Strategy 1: Look for significantly overloaded CPUs first
    let mut best_victim = None;
    let mut best_victim_load = 0;

    for cpu_id in 0..total_cpus {
        if cpu_id == current_cpu {
            continue;
        }

        if let Some(victim_data) = cpu_data(cpu_id) {
            let victim_load = victim_data.queue_length();

            // Prefer CPUs with high load for better load balancing
            if victim_load > 2 && victim_load > best_victim_load {
                best_victim_load = victim_load;
                best_victim = Some(cpu_id);
            }
        }
    }

    // Try to steal from the most loaded CPU first
    if let Some(victim_cpu) = best_victim {
        if let Some(victim_data) = cpu_data(victim_cpu) {
            let stolen_tasks = victim_data.steal_tasks(1);
            if let Some(task) = stolen_tasks.into_iter().next() {
                info!("Work steal: task {} from overloaded CPU{} (load={})",
                      task.pid(), victim_cpu, best_victim_load);

                // Update task's preferred CPU to current CPU for locality
                task.preferred_cpu.store(current_cpu, core::sync::atomic::Ordering::Relaxed);

                crate::sync::memory_barrier::full();
                return Some(task);
            }
        }
    }

    // Strategy 2: Round-robin search for any available tasks
    for i in 1..total_cpus {
        let victim_cpu = (current_cpu + i) % total_cpus;

        if let Some(victim_data) = cpu_data(victim_cpu) {
            let victim_load = victim_data.queue_length();

            // Only steal if victim has more than one task (to avoid ping-ponging)
            if victim_load > 1 {
                let stolen_tasks = victim_data.steal_tasks(1);
                if let Some(task) = stolen_tasks.into_iter().next() {
                    debug!("Round-robin steal: task {} from CPU{} (load={})",
                           task.pid(), victim_cpu, victim_load);

                    // Update task's preferred CPU for locality
                    task.preferred_cpu.store(current_cpu, core::sync::atomic::Ordering::Relaxed);

                    crate::sync::memory_barrier::full();
                    return Some(task);
                }
            }
        }
    }

    // Strategy 3: Check for any single task that can be stolen (only if really needed)
    static STEAL_ATTEMPTS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
    let attempts = STEAL_ATTEMPTS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);

    // Only try single-task stealing occasionally to avoid excessive stealing
    if attempts % 10 == 0 {
        for i in 1..total_cpus {
            let victim_cpu = (current_cpu + i) % total_cpus;

            if let Some(victim_data) = cpu_data(victim_cpu) {
                let victim_load = victim_data.queue_length();

                if victim_load == 1 {
                    let stolen_tasks = victim_data.steal_tasks(1);
                    if let Some(task) = stolen_tasks.into_iter().next() {
                        debug!("Last-resort steal: task {} from CPU{}", task.pid(), victim_cpu);

                        // Update task's preferred CPU for locality
                        task.preferred_cpu.store(current_cpu, core::sync::atomic::Ordering::Relaxed);

                        crate::sync::memory_barrier::full();
                        return Some(task);
                    }
                }
            }
        }
    }

    None
}

/// Enhanced task scheduling with preemption support
fn schedule_task_with_preemption(task: Arc<TaskControlBlock>) {
    let cpu_id = current_cpu_id();
    let cpu_data = match current_cpu_data() {
        Some(data) => data,
        None => {
            error!("CPU{}: No CPU data available for task execution", cpu_id);
            return;
        }
    };

    // Check if task should handle signals before execution
    if task.signal_state.lock().has_deliverable_signals() {
        handle_task_signals(&task);
        // If task was terminated by signal, don't execute it
        if task.is_zombie() {
            debug!("CPU{}: Task {} is zombie, skipping execution", cpu_id, task.pid());
            return;
        }
    }

    // 设置任务状态为运行中
    {
        let mut status = task.task_status.lock();
        if *status != TaskStatus::Ready {
            debug!("CPU{}: Task {} status is {:?}, not ready to run", cpu_id, task.pid(), *status);
            return;
        }
        *status = TaskStatus::Running;
    }

    let start_time = get_time_us();
    task.last_runtime.store(start_time, Ordering::Release);
    cpu_data.task_start_time.store(start_time, Ordering::Release);

    // 设置当前任务
    cpu_data.set_current_task(Some(task.clone()));

    // 获取任务上下文指针 - 关键：必须在设置当前任务之后
    let task_cx_ptr = {
        let task_context = task.mm.task_cx.lock();
        &*task_context as *const TaskContext
    };

    let idle_cx_ptr = {
        let mut idle_context = cpu_data.idle_context.lock();
        &mut *idle_context as *mut TaskContext
    };

    // 确保内存一致性
    crate::sync::memory_barrier::full();

    drop(cpu_data);

    // Switch to the task
    unsafe {
        __switch(idle_cx_ptr, task_cx_ptr);
    }

    // 任务执行完毕，返回到调度循环
    let end_time = get_time_us();
    let runtime = end_time.saturating_sub(start_time);

    // 更新任务统计信息
    task.sched.lock().update_vruntime(runtime);

    if let Some(cpu_data) = current_cpu_data() {
        cpu_data.record_task_execution(runtime, 0);
        // 清除当前任务指针，避免任务完成后仍被认为是当前任务
        cpu_data.set_current_task(None);
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

/// Enhanced idle state with intelligent task discovery and power management
fn enter_simple_idle_state() {
    let cpu_data = match current_cpu_data() {
        Some(data) => data,
        None => {
            error!("No CPU data available for idle, entering emergency idle loop");
            loop {
                ipi::handle_ipi_interrupt();

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
    let idle_start_time = get_time_msec();

    // Set CPU to idle state but keep it online for scheduling
    cpu_data.set_state(crate::smp::cpu::CpuState::Idle);

    debug!("CPU{} entering enhanced idle state", cpu_id);

    // Verify interrupt enable state
    let sstatus_val = riscv::register::sstatus::read();
    let sie_val = riscv::register::sie::read();
    debug!("CPU{} idle: sstatus.sie={}, sie.ssoft={}", cpu_id, sstatus_val.sie(), sie_val.ssoft());

    // Idle loop with multiple wake-up strategies
    let mut idle_iterations = 0;
    loop {
        idle_iterations += 1;

        // 1. Always process pending IPI messages first (highest priority)
        ipi::handle_ipi_interrupt();

        // 2. Check if we have local tasks after IPI processing
        let queue_len = cpu_data.queue_length();
        if queue_len > 0 {
            debug!("CPU{} waking up: found {} local tasks", cpu_id, queue_len);
            break;
        }

        // 3. Check if we need to reschedule (IPI might have set this flag)
        if cpu_data.need_resched() {
            debug!("CPU{} waking up: reschedule flag set", cpu_id);
            cpu_data.set_need_resched(false);
            break;
        }

        // 4. 关键修复：频繁检查全局任务池，特别是对于非CPU0的处理器
        if idle_iterations % 2 == 0 || cpu_id != 0 {  // 非CPU0更频繁检查
            if let Some(task) = crate::task::task_manager::fetch_task() {
                debug!("CPU{} waking up: found global task {}", cpu_id, task.pid());
                cpu_data.add_task(task);
                break;
            }
        }

        // 5. Try work stealing (less frequently to avoid overhead)
        if idle_iterations % 10 == 0 {
            if let Some(stolen_task) = try_traditional_work_stealing() {
                debug!("CPU{} waking up: stole task {} from another CPU", cpu_id, stolen_task.pid());
                cpu_data.add_task(stolen_task);
                break;
            }
        }

        // 6. Periodic idle statistics update
        let current_time = get_time_msec();
        if idle_iterations % 100 == 0 {
            let idle_duration = current_time.saturating_sub(idle_start_time);
            cpu_data.record_idle_time(idle_duration * 1000); // Convert to microseconds

            // Debug info every 10 seconds of idle time
            if idle_duration > 0 && idle_duration % 10000 == 0 {
                debug!("CPU{} has been idle for {}ms (iterations={})",
                       cpu_id, idle_duration, idle_iterations);
            }
        }

        // 7. Check for system-wide idle condition and help with load balancing
        if idle_iterations % 50 == 0 && cpu_id == 0 {
            // CPU0 can trigger global load balancing when idle
            crate::task::task_manager::perform_global_load_balance();
        }

        // 8. Critical fix: Replace WFI with active polling for secondary CPUs
        // The WFI instruction on RISC-V may not reliably wake up on software interrupts
        #[cfg(target_arch = "riscv64")]
        unsafe {
            // Ensure software interrupts are enabled
            riscv::register::sstatus::set_sie();
            riscv::register::sie::set_ssoft();

            // Debug: Check interrupt configuration before WFI
            let sstatus = riscv::register::sstatus::read();
            let sie = riscv::register::sie::read();
            let sip = riscv::register::sip::read();

            if idle_iterations % 10 == 0 && cpu_id > 0 {
                debug!("CPU{} WFI check: sstatus.sie={}, sie.ssoft={}, sip.ssoft={}",
                       cpu_id, sstatus.sie(), sie.ssoft(), sip.ssoft());

                // Check if we already have a pending software interrupt
                if sip.ssoft() {
                    info!("CPU{} has pending software interrupt BEFORE WFI!", cpu_id);
                }
            }

            // Check one more time before WFI
            let sip_before = riscv::register::sip::read();
            if sip_before.ssoft() && cpu_id > 0 {
                info!("CPU{} skipping WFI - software interrupt already pending", cpu_id);
                // Process the interrupt immediately instead of WFI
                continue;
            }

            // Use WFI for power efficiency (should work now with proper interrupt setup)
            if cpu_id > 0 && idle_iterations % 50 == 0 {
                debug!("CPU{} entering WFI (iteration {})", cpu_id, idle_iterations);
            }

            riscv::asm::wfi();

            // Check if WFI was interrupted by software interrupt
            if cpu_id > 0 {
                let sip_after = riscv::register::sip::read();
                if sip_after.ssoft() {
                    info!("CPU{} WFI interrupted by software interrupt!", cpu_id);
                }
            }
        }

        #[cfg(not(target_arch = "riscv64"))]
        {
            // Yield to other threads/processes
            core::hint::spin_loop();

            // Add a small delay to reduce CPU usage
            for _ in 0..1000 {
                core::hint::spin_loop();
            }
        }

        // 9. Timeout check - don't stay idle forever if there might be work
        let idle_duration = current_time.saturating_sub(idle_start_time);
        if idle_duration > 5000 { // 5 seconds max idle time
            // Force a more aggressive check for work
            if let Some(task) = crate::task::task_manager::fetch_task() {
                debug!("CPU{} found task {} after timeout", cpu_id, task.pid());
                cpu_data.add_task(task);
                break;
            }
        }
    }

    // Record total idle time
    let total_idle_time = get_time_msec().saturating_sub(idle_start_time);
    cpu_data.record_idle_time(total_idle_time * 1000); // Convert to microseconds

    // Set CPU back to online state
    cpu_data.set_state(crate::smp::cpu::CpuState::Online);

    debug!("CPU{} exiting idle state after {}ms ({} iterations)",
           cpu_id, total_idle_time, idle_iterations);
}

pub fn suspend_current_and_run_next() {
    let task = take_current_task().expect("No current task to suspend");
    let cpu_id = current_cpu_id();
    let end_time = get_time_us();
    let runtime = end_time.saturating_sub(task.last_runtime.load(Ordering::Relaxed));

    // Update task statistics
    task.sched.lock().update_vruntime(runtime);

    let (task_cx_ptr, should_readd) = {
        let mut task_cx = task.mm.task_cx.lock();
        let task_cx_ptr = &mut *task_cx as *mut TaskContext;
        let mut task_status = task.task_status.lock();

        // 关键修复：检查任务是否真的应该继续运行
        let should_readd = match *task_status {
            TaskStatus::Running => {
                // 只有明确仍在运行状态的任务才重新加入队列
                *task_status = TaskStatus::Ready;
                true
            },
            TaskStatus::Sleeping | TaskStatus::Zombie => {
                // 已经睡眠或僵尸状态的任务不应该重新调度
                debug!("CPU{}: Task {} status is {:?}, not readding to queue", cpu_id, task.pid(), *task_status);
                false
            },
            _ => {
                debug!("CPU{}: Task {} status is {:?}, not readding to queue", cpu_id, task.pid(), *task_status);
                false
            }
        };

        drop(task_status);
        (task_cx_ptr, should_readd)
    };

    // If task should continue running, add it back to the queue
    if should_readd {
        if let Some(cpu_data) = current_cpu_data() {
            cpu_data.add_task(task);
        }
        // 确保任务状态更改可见
        crate::sync::memory_barrier::full();
    } else {
        debug!("CPU{}: Task {} suspended without readding to queue", cpu_id, task.pid());
    }

    // 按照工作版本逻辑：直接调用 schedule 函数切换到 idle
    schedule(task_cx_ptr);
}

/// Block the current task and run the next task (按工作版本逻辑重构)
pub fn block_current_and_run_next() {
    let task = take_current_task().expect("No current task to block");
    let cpu_id = current_cpu_id();
    let end_time = get_time_us();
    let runtime = end_time.saturating_sub(task.last_runtime.load(Ordering::Relaxed));

    debug!("CPU{}: Blocking task {} after {}us runtime", cpu_id, task.pid(), runtime);

    // Update task statistics
    task.sched.lock().update_vruntime(runtime);
    let task_cx_ptr = &mut *task.mm.task_cx.lock() as *mut TaskContext;

    // Task is blocked, don't add back to queue
    debug!("CPU{}: Task {} blocked, not readding to queue", cpu_id, task.pid());

    // 按照工作版本逻辑：直接调用 schedule 函数切换到 idle
    schedule(task_cx_ptr);
}

/// Exit the current task and run the next task (按工作版本逻辑重构)
pub fn exit_current_and_run_next(exit_code: i32) {
    let task = take_current_task().expect("No current task to exit");
    let cpu_id = current_cpu_id();
    let pid = task.pid();

    debug!("CPU{}: Task {} exiting with code {}", cpu_id, pid, exit_code);

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