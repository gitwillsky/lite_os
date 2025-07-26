/// Per-CPU data structures and management
/// 
/// This module defines the per-CPU data structure that contains all the
/// CPU-local state including scheduler queues, memory pools, and statistics.

use alloc::{sync::Arc, vec::Vec, collections::VecDeque};
use core::sync::atomic::{AtomicUsize, AtomicU64, AtomicBool, Ordering};
use crate::{
    sync::spinlock::SpinLock,
    task::{TaskControlBlock, context::TaskContext},
    memory::slab_allocator::SlabAllocator,
    timer::TimeSpec,
};

/// Type of CPU in the system
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CpuType {
    /// Bootstrap processor (the first CPU that boots)
    Bootstrap,
    /// Application processor (secondary CPUs)
    Application,
}

/// CPU state
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CpuState {
    /// CPU is offline
    Offline,
    /// CPU is coming online
    Starting,
    /// CPU is online and active
    Online,
    /// CPU is going offline
    Stopping,
    /// CPU is in idle state
    Idle,
    /// CPU is in error state
    Error,
}

/// Per-CPU scheduler queue
#[derive(Debug)]
pub struct CpuSchedulerQueue {
    /// High priority task queue
    pub high_priority: VecDeque<Arc<TaskControlBlock>>,
    /// Normal priority task queue  
    pub normal_priority: VecDeque<Arc<TaskControlBlock>>,
    /// Low priority task queue
    pub low_priority: VecDeque<Arc<TaskControlBlock>>,
    /// CFS red-black tree (will be implemented later)
    pub cfs_queue: VecDeque<Arc<TaskControlBlock>>,
    /// Number of tasks in all queues
    pub task_count: usize,
}

impl CpuSchedulerQueue {
    pub fn new() -> Self {
        Self {
            high_priority: VecDeque::new(),
            normal_priority: VecDeque::new(),
            low_priority: VecDeque::new(),
            cfs_queue: VecDeque::new(),
            task_count: 0,
        }
    }

    /// Add a task to the appropriate queue based on its priority
    pub fn add_task(&mut self, task: Arc<TaskControlBlock>) {
        let priority = task.sched.lock().priority;
        
        match priority {
            p if p > 120 => self.low_priority.push_back(task),
            p if p > 100 => self.normal_priority.push_back(task),
            _ => self.high_priority.push_back(task),
        }
        
        self.task_count += 1;
    }

    /// Get the next task to run
    pub fn pop_task(&mut self) -> Option<Arc<TaskControlBlock>> {
        if let Some(task) = self.high_priority.pop_front() {
            self.task_count -= 1;
            return Some(task);
        }
        
        if let Some(task) = self.normal_priority.pop_front() {
            self.task_count -= 1;
            return Some(task);
        }
        
        if let Some(task) = self.cfs_queue.pop_front() {
            self.task_count -= 1;
            return Some(task);
        }
        
        if let Some(task) = self.low_priority.pop_front() {
            self.task_count -= 1;
            return Some(task);
        }
        
        None
    }

    /// Get the number of tasks in the queue
    pub fn len(&self) -> usize {
        self.task_count
    }

    /// Check if the queue is empty
    pub fn is_empty(&self) -> bool {
        self.task_count == 0
    }

    /// Steal half of the tasks for load balancing
    pub fn steal_tasks(&mut self, count: usize) -> Vec<Arc<TaskControlBlock>> {
        let mut stolen = Vec::new();
        let mut remaining = count;

        // Steal from low priority first
        while remaining > 0 && !self.low_priority.is_empty() {
            if let Some(task) = self.low_priority.pop_back() {
                stolen.push(task);
                self.task_count -= 1;
                remaining -= 1;
            }
        }

        // Then from normal priority
        while remaining > 0 && !self.normal_priority.is_empty() {
            if let Some(task) = self.normal_priority.pop_back() {
                stolen.push(task);
                self.task_count -= 1;
                remaining -= 1;
            }
        }

        // Don't steal high priority tasks to maintain responsiveness
        stolen
    }
}

/// CPU load statistics
#[derive(Debug)]
pub struct CpuLoadStats {
    /// Number of tasks executed
    pub tasks_executed: AtomicU64,
    /// Total CPU time spent in user mode (microseconds)
    pub user_time: AtomicU64,
    /// Total CPU time spent in kernel mode (microseconds)
    pub kernel_time: AtomicU64,
    /// Total idle time (microseconds)
    pub idle_time: AtomicU64,
    /// Load average (scaled by 1000)
    pub load_avg_1min: AtomicUsize,
    pub load_avg_5min: AtomicUsize,
    pub load_avg_15min: AtomicUsize,
    /// Number of context switches
    pub context_switches: AtomicU64,
    /// Number of interrupts handled
    pub interrupts_handled: AtomicU64,
}

impl CpuLoadStats {
    pub fn new() -> Self {
        Self {
            tasks_executed: AtomicU64::new(0),
            user_time: AtomicU64::new(0),
            kernel_time: AtomicU64::new(0),
            idle_time: AtomicU64::new(0),
            load_avg_1min: AtomicUsize::new(0),
            load_avg_5min: AtomicUsize::new(0),
            load_avg_15min: AtomicUsize::new(0),
            context_switches: AtomicU64::new(0),
            interrupts_handled: AtomicU64::new(0),
        }
    }

    /// Update CPU load statistics
    pub fn update_load(&self, queue_length: usize) {
        // Simple exponential moving average
        let current_load = (queue_length * 1000) as usize; // Scale by 1000
        
        // 1-minute load average (alpha = 1 - exp(-1/60))
        let alpha_1min = 16; // Approximation of (1 - exp(-1/60)) * 1000
        let old_load = self.load_avg_1min.load(Ordering::Relaxed);
        let new_load = (old_load * (1000 - alpha_1min) + current_load * alpha_1min) / 1000;
        self.load_avg_1min.store(new_load, Ordering::Relaxed);

        // 5-minute load average (alpha = 1 - exp(-1/300))
        let alpha_5min = 3; // Approximation of (1 - exp(-1/300)) * 1000
        let old_load = self.load_avg_5min.load(Ordering::Relaxed);
        let new_load = (old_load * (1000 - alpha_5min) + current_load * alpha_5min) / 1000;
        self.load_avg_5min.store(new_load, Ordering::Relaxed);

        // 15-minute load average (alpha = 1 - exp(-1/900))
        let alpha_15min = 1; // Approximation of (1 - exp(-1/900)) * 1000
        let old_load = self.load_avg_15min.load(Ordering::Relaxed);
        let new_load = (old_load * (1000 - alpha_15min) + current_load * alpha_15min) / 1000;
        self.load_avg_15min.store(new_load, Ordering::Relaxed);
    }

    /// Get current CPU utilization percentage (0-100)
    pub fn cpu_utilization(&self) -> u32 {
        let total_time = self.user_time.load(Ordering::Relaxed) 
                       + self.kernel_time.load(Ordering::Relaxed) 
                       + self.idle_time.load(Ordering::Relaxed);
        
        if total_time == 0 {
            return 0;
        }
        
        let active_time = self.user_time.load(Ordering::Relaxed) 
                        + self.kernel_time.load(Ordering::Relaxed);
        
        ((active_time * 100) / total_time) as u32
    }
}

/// Per-CPU data structure
/// 
/// This structure contains all CPU-local state including the scheduler queue,
/// current running task, statistics, and CPU-local memory allocator.
pub struct CpuData {
    /// CPU ID
    pub cpu_id: usize,
    
    /// CPU type (bootstrap or application processor)
    pub cpu_type: CpuType,
    
    /// Current CPU state
    pub state: SpinLock<CpuState>,
    
    /// Architecture-specific CPU ID (e.g., HART ID for RISC-V)
    pub arch_cpu_id: AtomicUsize,
    
    /// Current running task
    pub current_task: SpinLock<Option<Arc<TaskControlBlock>>>,
    
    /// Per-CPU scheduler queue
    pub scheduler_queue: SpinLock<CpuSchedulerQueue>,
    
    /// Idle task context for this CPU
    pub idle_context: SpinLock<TaskContext>,
    
    /// Per-CPU memory allocator
    pub allocator: SpinLock<Option<SlabAllocator>>,
    
    /// CPU load statistics
    pub stats: CpuLoadStats,
    
    /// Timestamp when this CPU was last idle
    pub last_idle_time: AtomicU64,
    
    /// Timestamp when this CPU started executing current task
    pub task_start_time: AtomicU64,
    
    /// CPU frequency in Hz
    pub frequency: AtomicU64,
    
    /// Flag indicating if this CPU needs rescheduling
    pub need_resched: AtomicBool,
    
    /// Flag indicating if this CPU is in interrupt context
    pub in_interrupt: AtomicBool,
    
    /// Interrupt nesting level
    pub interrupt_nesting: AtomicUsize,
    
    /// Cache line alignment to avoid false sharing
    _padding: [u8; 64],
}

impl CpuData {
    /// Create new per-CPU data structure
    pub fn new(cpu_id: usize, cpu_type: CpuType) -> Self {
        Self {
            cpu_id,
            cpu_type,
            state: SpinLock::new(CpuState::Offline),
            arch_cpu_id: AtomicUsize::new(cpu_id), // Default to logical ID, can be overridden
            current_task: SpinLock::new(None),
            scheduler_queue: SpinLock::new(CpuSchedulerQueue::new()),
            idle_context: SpinLock::new(TaskContext::zero_init()),
            allocator: SpinLock::new(None),
            stats: CpuLoadStats::new(),
            last_idle_time: AtomicU64::new(0),
            task_start_time: AtomicU64::new(0),
            frequency: AtomicU64::new(1_000_000_000), // Default 1GHz
            need_resched: AtomicBool::new(false),
            in_interrupt: AtomicBool::new(false),
            interrupt_nesting: AtomicUsize::new(0),
            _padding: [0; 64],
        }
    }

    /// Set the architecture-specific CPU ID
    pub fn set_arch_cpu_id(&self, arch_id: usize) {
        self.arch_cpu_id.store(arch_id, Ordering::Relaxed);
    }

    /// Get the current CPU state
    pub fn state(&self) -> CpuState {
        *self.state.lock()
    }

    /// Set the CPU state
    pub fn set_state(&self, new_state: CpuState) {
        *self.state.lock() = new_state;
    }

    /// Check if this CPU needs rescheduling
    pub fn need_resched(&self) -> bool {
        self.need_resched.load(Ordering::Acquire)
    }

    /// Set the reschedule flag
    pub fn set_need_resched(&self, need: bool) {
        self.need_resched.store(need, Ordering::Release);
    }

    /// Enter interrupt context
    pub fn enter_interrupt(&self) {
        self.in_interrupt.store(true, Ordering::Release);
        self.interrupt_nesting.fetch_add(1, Ordering::AcqRel);
    }

    /// Exit interrupt context
    pub fn exit_interrupt(&self) {
        let nesting = self.interrupt_nesting.fetch_sub(1, Ordering::AcqRel);
        if nesting <= 1 {
            self.in_interrupt.store(false, Ordering::Release);
        }
    }

    /// Check if in interrupt context
    pub fn in_interrupt(&self) -> bool {
        self.in_interrupt.load(Ordering::Acquire)
    }

    /// Get the current task running on this CPU
    pub fn current_task(&self) -> Option<Arc<TaskControlBlock>> {
        self.current_task.lock().clone()
    }

    /// Set the current task for this CPU
    pub fn set_current_task(&self, task: Option<Arc<TaskControlBlock>>) {
        *self.current_task.lock() = task;
    }

    /// Add a task to this CPU's scheduler queue
    pub fn add_task(&self, task: Arc<TaskControlBlock>) {
        self.scheduler_queue.lock().add_task(task);
        self.set_need_resched(true);
    }

    /// Get the next task to run from this CPU's queue
    pub fn pop_task(&self) -> Option<Arc<TaskControlBlock>> {
        self.scheduler_queue.lock().pop_task()
    }

    /// Get the number of tasks in this CPU's queue
    pub fn queue_length(&self) -> usize {
        self.scheduler_queue.lock().len()
    }

    /// Calculate the load of this CPU (queue length + current task)
    pub fn load(&self) -> usize {
        let queue_len = self.queue_length();
        let current_task_load = if self.current_task().is_some() { 1 } else { 0 };
        queue_len + current_task_load
    }

    /// Steal tasks from this CPU for load balancing
    pub fn steal_tasks(&self, count: usize) -> Vec<Arc<TaskControlBlock>> {
        self.scheduler_queue.lock().steal_tasks(count)
    }

    /// Record task execution statistics
    pub fn record_task_execution(&self, user_time: u64, kernel_time: u64) {
        self.stats.tasks_executed.fetch_add(1, Ordering::Relaxed);
        self.stats.user_time.fetch_add(user_time, Ordering::Relaxed);
        self.stats.kernel_time.fetch_add(kernel_time, Ordering::Relaxed);
        self.stats.context_switches.fetch_add(1, Ordering::Relaxed);
    }

    /// Record idle time
    pub fn record_idle_time(&self, idle_time: u64) {
        self.stats.idle_time.fetch_add(idle_time, Ordering::Relaxed);
    }

    /// Update load statistics
    pub fn update_load_stats(&self) {
        self.stats.update_load(self.queue_length());
    }
}

/// CPU information structure for topology discovery
#[derive(Debug, Clone)]
pub struct CpuInfo {
    /// Logical CPU ID
    pub cpu_id: usize,
    /// Architecture-specific ID (e.g., HART ID for RISC-V, APIC ID for x86)
    pub arch_id: usize,
    /// CPU frequency in Hz
    pub frequency: u64,
    /// NUMA node this CPU belongs to
    pub numa_node: usize,
    /// CPU features/capabilities
    pub features: CpuFeatures,
}

/// CPU features and capabilities
#[derive(Debug, Clone, Default)]
pub struct CpuFeatures {
    /// Supports floating point operations
    pub has_fpu: bool,
    /// Supports vector operations
    pub has_vector: bool,
    /// Cache sizes (L1I, L1D, L2, L3) in bytes
    pub cache_sizes: [usize; 4],
    /// TLB sizes
    pub tlb_sizes: [usize; 2], // [ITLB, DTLB]
}

impl CpuInfo {
    pub fn new(cpu_id: usize, arch_id: usize) -> Self {
        Self {
            cpu_id,
            arch_id,
            frequency: 1_000_000_000, // Default 1GHz
            numa_node: 0, // Default to node 0
            features: CpuFeatures::default(),
        }
    }
}