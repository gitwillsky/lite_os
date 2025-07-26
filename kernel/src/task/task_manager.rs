/// Multi-core task manager implementation
///
/// This module manages the global task pool and coordinates task distribution
/// across multiple CPUs. It implements load balancing and task affinity.

use alloc::{
    boxed::Box,
    collections::{binary_heap::BinaryHeap, vec_deque::VecDeque},
    sync::Arc,
    vec::Vec,
};
use core::{cmp::Ordering, sync::atomic::AtomicUsize};

use crate::{
    smp::{current_cpu_id, cpu_data, cpu_count, cpu_is_online},
    sync::{SpinLock, RwSpinLock},
    task::{
        current_task,
        pid::INIT_PID,
        scheduler::{
            Scheduler, cfs_scheduler::CFScheduler, fifo_scheduler::FIFOScheduler,
            priority_scheduler::PriorityScheduler,
        },
        task::{TaskControlBlock, TaskStatus},
    },
};

/// Scheduling policy enumeration
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchedulingPolicy {
    FIFO,       // First In, First Out
    Priority,   // Priority-based scheduling
    RoundRobin, // Round-robin time slicing
    CFS,        // Completely Fair Scheduler
}

impl Default for SchedulingPolicy {
    fn default() -> Self {
        SchedulingPolicy::CFS
    }
}

/// Global task manager for coordinating tasks across all CPUs
struct GlobalTaskManager {
    /// Current scheduling policy
    policy: RwSpinLock<SchedulingPolicy>,

    /// Global task pool for new tasks and load balancing
    global_task_pool: SpinLock<VecDeque<Arc<TaskControlBlock>>>,

    /// Reference to the init process
    init_proc: SpinLock<Option<Arc<TaskControlBlock>>>,

    /// Global task statistics
    total_tasks: AtomicUsize,
    created_tasks: AtomicUsize,
    completed_tasks: AtomicUsize,
}

impl GlobalTaskManager {
    const fn new() -> Self {
        Self {
            policy: RwSpinLock::new(SchedulingPolicy::CFS),
            global_task_pool: SpinLock::new(VecDeque::new()),
            init_proc: SpinLock::new(None),
            total_tasks: AtomicUsize::new(0),
            created_tasks: AtomicUsize::new(0),
            completed_tasks: AtomicUsize::new(0),
        }
    }

    /// Add a new task to the system
    fn add_task(&self, task: Arc<TaskControlBlock>) {
        // Track init process specially
        if task.pid() == INIT_PID {
            *self.init_proc.lock() = Some(task.clone());
        }

        // Update statistics
        self.total_tasks.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        self.created_tasks.fetch_add(1, core::sync::atomic::Ordering::Relaxed);

        // Try to place task on the best CPU
        let target_cpu = self.select_best_cpu_for_task(&task);

        if let Some(cpu_data) = cpu_data(target_cpu) {
            debug!("Adding task {} to CPU {}", task.pid(), target_cpu);
            cpu_data.add_task(task);
        } else {
            // Fallback to global pool
            self.global_task_pool.lock().push_back(task);
            warn!("Added task to global pool (no CPU data available)");
        }
    }

    /// Select the best CPU for a new task
    fn select_best_cpu_for_task(&self, task: &Arc<TaskControlBlock>) -> usize {
        let current_cpu = current_cpu_id();
        let total_cpus = cpu_count();

        // Check task's CPU affinity if set
        let affinity = task.cpu_affinity.lock();
        let preferred_cpu = task.preferred_cpu.load(core::sync::atomic::Ordering::Relaxed);
        drop(affinity);

        // If task has a preferred CPU and it's online, try to use it
        if preferred_cpu < total_cpus && cpu_is_online(preferred_cpu) {
            if let Some(cpu_data) = cpu_data(preferred_cpu) {
                if cpu_data.load() < 4 { // Don't overload the preferred CPU
                    return preferred_cpu;
                }
            }
        }

        // Find the CPU with the lowest load
        let mut best_cpu = current_cpu;
        let mut lowest_load = usize::MAX;

        for cpu_id in 0..total_cpus {
            if !cpu_is_online(cpu_id) {
                continue;
            }

            if let Some(cpu_data) = cpu_data(cpu_id) {
                let load = cpu_data.load();
                if load < lowest_load {
                    lowest_load = load;
                    best_cpu = cpu_id;
                }
            }
        }

        best_cpu
    }

    /// Get a task from the global pool (used for load balancing)
    fn get_global_task(&self) -> Option<Arc<TaskControlBlock>> {
        self.global_task_pool.lock().pop_front()
    }

    /// Get the init process reference
    fn init_proc(&self) -> Option<Arc<TaskControlBlock>> {
        self.init_proc.lock().clone()
    }

    /// Set the global scheduling policy
    fn set_scheduling_policy(&self, policy: SchedulingPolicy) {
        *self.policy.write() = policy;
        info!("Scheduling policy changed to {:?}", policy);
    }

    /// Get the current scheduling policy
    fn get_scheduling_policy(&self) -> SchedulingPolicy {
        *self.policy.read()
    }

    /// Get total number of tasks in the system
    fn total_task_count(&self) -> usize {
        let mut total = 0;

        // Count tasks in global pool
        total += self.global_task_pool.lock().len();

        // Count tasks in per-CPU queues
        for cpu_id in 0..cpu_count() {
            if let Some(cpu_data) = cpu_data(cpu_id) {
                total += cpu_data.load();
            }
        }

        total
    }

    /// Find a task by PID across all CPUs
    fn find_task_by_pid(&self, pid: usize) -> Option<Arc<TaskControlBlock>> {
        // Check if it's the current task on any CPU
        for cpu_id in 0..cpu_count() {
            if let Some(cpu_data) = cpu_data(cpu_id) {
                if let Some(current_task) = cpu_data.current_task() {
                    if current_task.pid() == pid {
                        return Some(current_task);
                    }
                }
            }
        }

        // Check global pool
        for task in self.global_task_pool.lock().iter() {
            if task.pid() == pid {
                return Some(task.clone());
            }
        }

        // Check per-CPU queues (this is expensive, but necessary for correctness)
        for cpu_id in 0..cpu_count() {
            if let Some(cpu_data) = cpu_data(cpu_id) {
                let queue = cpu_data.scheduler_queue.lock();

                // Check all priority queues
                for task in queue.high_priority.iter()
                    .chain(queue.normal_priority.iter())
                    .chain(queue.low_priority.iter())
                    .chain(queue.cfs_queue.iter()) {
                    if task.pid() == pid {
                        return Some(task.clone());
                    }
                }
            }
        }

        None
    }

    /// Get all tasks in the system (for debugging)
    fn get_all_tasks(&self) -> Vec<Arc<TaskControlBlock>> {
        let mut tasks = Vec::new();

        // Add current tasks from each CPU
        for cpu_id in 0..cpu_count() {
            if let Some(cpu_data) = cpu_data(cpu_id) {
                if let Some(current_task) = cpu_data.current_task() {
                    tasks.push(current_task);
                }
            }
        }

        // Add tasks from global pool
        for task in self.global_task_pool.lock().iter() {
            tasks.push(task.clone());
        }

        // Add tasks from per-CPU queues
        for cpu_id in 0..cpu_count() {
            if let Some(cpu_data) = cpu_data(cpu_id) {
                let queue = cpu_data.scheduler_queue.lock();

                for task in queue.high_priority.iter()
                    .chain(queue.normal_priority.iter())
                    .chain(queue.low_priority.iter())
                    .chain(queue.cfs_queue.iter()) {
                    tasks.push(task.clone());
                }
            }
        }

        tasks
    }

    /// Perform global load balancing
    fn global_load_balance(&self) {
        let total_cpus = cpu_count();
        if total_cpus <= 1 {
            return; // No balancing needed for single CPU
        }

        // Collect load information
        let mut cpu_loads = Vec::new();
        for cpu_id in 0..total_cpus {
            if cpu_is_online(cpu_id) {
                if let Some(cpu_data) = cpu_data(cpu_id) {
                    cpu_loads.push((cpu_id, cpu_data.load()));
                }
            }
        }

        if cpu_loads.is_empty() {
            return;
        }

        // Sort by load (highest first)
        cpu_loads.sort_by(|a, b| b.1.cmp(&a.1));

        let avg_load = cpu_loads.iter().map(|(_, load)| load).sum::<usize>() / cpu_loads.len();

        // Balance load from highest to lowest
        for &(overloaded_cpu, load) in cpu_loads.iter() {
            if load <= avg_load + 1 {
                break; // No more significantly overloaded CPUs
            }

            // Find an underloaded CPU
            if let Some(&(underloaded_cpu, _)) = cpu_loads.iter()
                .find(|(_, l)| *l < avg_load.saturating_sub(1)) {

                // Move tasks from overloaded to underloaded CPU
                if let Some(overloaded_data) = cpu_data(overloaded_cpu) {
                    let tasks_to_move = (load - avg_load) / 2;
                    let stolen_tasks = overloaded_data.steal_tasks(tasks_to_move);

                    for task in stolen_tasks {
                        if let Some(underloaded_data) = cpu_data(underloaded_cpu) {
                            underloaded_data.add_task(task);

                            // Send reschedule IPI to wake up the underloaded CPU
                            let _ = crate::smp::ipi::send_reschedule_ipi(underloaded_cpu);
                        }
                    }

                    debug!("Balanced {} tasks from CPU{} to CPU{}",
                           tasks_to_move, overloaded_cpu, underloaded_cpu);
                }
            }
        }

        // Distribute tasks from global pool
        let mut global_pool = self.global_task_pool.lock();
        while let Some(task) = global_pool.pop_front() {
            let target_cpu = self.select_best_cpu_for_task(&task);
            if let Some(cpu_data) = cpu_data(target_cpu) {
                cpu_data.add_task(task);
                let _ = crate::smp::ipi::send_reschedule_ipi(target_cpu);
            } else {
                // Put it back if no CPU is available
                global_pool.push_back(task);
                break;
            }
        }
    }
}

/// Global task manager instance
static GLOBAL_TASK_MANAGER: GlobalTaskManager = GlobalTaskManager::new();

/// Add a task to the system
pub fn add_task(task: Arc<TaskControlBlock>) {
    GLOBAL_TASK_MANAGER.add_task(task);
}

/// Fetch a task from the current CPU's queue
pub fn fetch_task() -> Option<Arc<TaskControlBlock>> {
    let cpu_id = current_cpu_id();

    // First try to get a task from the current CPU's queue
    if let Some(cpu_data) = cpu_data(cpu_id) {
        if let Some(task) = cpu_data.pop_task() {
            return Some(task);
        }
    }

    // If no local task, try the global pool
    GLOBAL_TASK_MANAGER.get_global_task()
}

/// Set the global scheduling policy
pub fn set_scheduling_policy(policy: SchedulingPolicy) {
    GLOBAL_TASK_MANAGER.set_scheduling_policy(policy);
}

/// Get the current scheduling policy
pub fn get_scheduling_policy() -> SchedulingPolicy {
    GLOBAL_TASK_MANAGER.get_scheduling_policy()
}

/// Get the init process
pub fn init_proc() -> Option<Arc<TaskControlBlock>> {
    GLOBAL_TASK_MANAGER.init_proc()
}

/// Find a task by PID
pub fn find_task_by_pid(pid: usize) -> Option<Arc<TaskControlBlock>> {
    GLOBAL_TASK_MANAGER.find_task_by_pid(pid)
}

/// Get all tasks in the system
pub fn get_all_tasks() -> Vec<Arc<TaskControlBlock>> {
    GLOBAL_TASK_MANAGER.get_all_tasks()
}

/// Get the total number of schedulable tasks
pub fn schedulable_task_count() -> usize {
    GLOBAL_TASK_MANAGER.total_task_count()
}

/// Perform global load balancing (called periodically by the load balancer)
pub fn perform_global_load_balance() {
    GLOBAL_TASK_MANAGER.global_load_balance();
}

/// Get task manager statistics
pub fn get_task_statistics() -> (usize, usize, usize) {
    let total = GLOBAL_TASK_MANAGER.total_tasks.load(core::sync::atomic::Ordering::Relaxed);
    let created = GLOBAL_TASK_MANAGER.created_tasks.load(core::sync::atomic::Ordering::Relaxed);
    let completed = GLOBAL_TASK_MANAGER.completed_tasks.load(core::sync::atomic::Ordering::Relaxed);
    (total, created, completed)
}

/// Print task manager debug information
pub fn print_task_manager_info() {
    let (total, created, completed) = get_task_statistics();
    let schedulable = schedulable_task_count();

    info!("=== Task Manager Statistics ===");
    info!("Total tasks: {}", total);
    info!("Created tasks: {}", created);
    info!("Completed tasks: {}", completed);
    info!("Schedulable tasks: {}", schedulable);
    info!("Policy: {:?}", get_scheduling_policy());

    // Print per-CPU load information
    info!("Per-CPU Loads:");
    for cpu_id in 0..cpu_count() {
        if cpu_is_online(cpu_id) {
            if let Some(cpu_data) = cpu_data(cpu_id) {
                let load = cpu_data.load();
                let queue_len = cpu_data.queue_length();
                let current_task_pid = cpu_data.current_task()
                    .map(|t| t.pid())
                    .unwrap_or(0);

                info!("  CPU{}: load={}, queue={}, current_task={}",
                      cpu_id, load, queue_len, current_task_pid);
            }
        }
    }

    info!("==============================");
}

/// Task affinity management
pub mod affinity {
    use super::*;
    use crate::smp::topology;

    /// Set CPU affinity for a task
    pub fn set_task_affinity(task: &Arc<TaskControlBlock>, cpu_mask: u64) -> Result<(), &'static str> {
        if cpu_mask == 0 {
            return Err("Invalid CPU mask");
        }

        // Validate that at least one CPU in the mask is online
        let mut valid_cpu_found = false;
        for cpu_id in 0..64 {
            if (cpu_mask & (1 << cpu_id)) != 0 && cpu_is_online(cpu_id) {
                valid_cpu_found = true;
                break;
            }
        }

        if !valid_cpu_found {
            return Err("No online CPU in affinity mask");
        }

        // Set the affinity mask
        task.cpu_affinity.lock().mask = cpu_mask;

        // Update preferred CPU to the first CPU in the mask
        for cpu_id in 0..64 {
            if (cpu_mask & (1 << cpu_id)) != 0 && cpu_is_online(cpu_id) {
                task.preferred_cpu.store(cpu_id, core::sync::atomic::Ordering::Relaxed);
                break;
            }
        }

        debug!("Set CPU affinity for task {} to mask {:#x}",
               task.pid(), cpu_mask);

        Ok(())
    }

    /// Get CPU affinity for a task
    pub fn get_task_affinity(task: &Arc<TaskControlBlock>) -> u64 {
        task.cpu_affinity.lock().mask
    }

    /// Set task affinity to a specific CPU
    pub fn bind_task_to_cpu(task: &Arc<TaskControlBlock>, cpu_id: usize) -> Result<(), &'static str> {
        if cpu_id >= 64 {
            return Err("CPU ID too large");
        }

        if !cpu_is_online(cpu_id) {
            return Err("CPU is not online");
        }

        set_task_affinity(task, 1 << cpu_id)
    }

    /// Set task affinity to CPUs sharing the same cache
    pub fn bind_task_to_cache_domain(task: &Arc<TaskControlBlock>, cpu_id: usize) -> Result<(), &'static str> {
        if cpu_id >= cpu_count() {
            return Err("Invalid CPU ID");
        }

        let cache_cpus = topology::cpus_sharing_cache_level(cpu_id, 2); // L2 cache
        let mut mask = 0u64;

        for &cache_cpu in &cache_cpus {
            if cache_cpu < 64 && cpu_is_online(cache_cpu) {
                mask |= 1 << cache_cpu;
            }
        }

        if mask == 0 {
            return Err("No online CPUs in cache domain");
        }

        set_task_affinity(task, mask)
    }

    /// Set task affinity to NUMA node
    pub fn bind_task_to_numa_node(task: &Arc<TaskControlBlock>, cpu_id: usize) -> Result<(), &'static str> {
        if cpu_id >= cpu_count() {
            return Err("Invalid CPU ID");
        }

        let numa_cpus = topology::cpus_in_same_numa_node(cpu_id);
        let mut mask = 0u64;

        for &numa_cpu in &numa_cpus {
            if numa_cpu < 64 && cpu_is_online(numa_cpu) {
                mask |= 1 << numa_cpu;
            }
        }

        if mask == 0 {
            return Err("No online CPUs in NUMA node");
        }

        set_task_affinity(task, mask)
    }
}

/// Re-export the CpuSet type for task affinity
pub use crate::task::task::CpuSet;