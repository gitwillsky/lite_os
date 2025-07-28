/// Inter-Processor Interrupt (IPI) support for SMP systems
///
/// This module provides the infrastructure for CPU-to-CPU communication
/// through interrupts, enabling coordination between processors.

use alloc::{boxed::Box, collections::{VecDeque, BTreeMap}, sync::Arc};
use core::sync::atomic::{AtomicUsize, AtomicBool, AtomicU64, Ordering};
use core::time::Duration;
use crate::{
    sync::spinlock::SpinLock,
    smp::{MAX_CPU_NUM, current_cpu_id, cpu_is_online},
    arch::sbi,
    memory::TlbManager,
    timer::get_time_msec,
};

/// IPI message priorities
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum IpiPriority {
    /// Critical messages that must be processed immediately
    Critical = 0,
    /// High priority messages (TLB flush, stop)
    High = 1,
    /// Normal priority messages (reschedule, function calls)
    Normal = 2,
    /// Low priority messages (wakeup, generic)
    Low = 3,
}

/// IPI response types for synchronous calls
#[derive(Debug, Clone)]
pub enum IpiResponse {
    /// Operation completed successfully
    Success,
    /// Operation completed with a result value
    Value(usize),
    /// Operation failed with error message
    Error(&'static str),
    /// Operation timed out
    Timeout,
}

/// Synchronous IPI call context
#[derive(Debug)]
struct SyncIpiCall {
    /// Unique call ID
    call_id: u64,
    /// Response from target CPU
    response: Option<IpiResponse>,
    /// Completion flag
    completed: AtomicBool,
    /// Timeout timestamp
    timeout_ms: u64,
}

impl SyncIpiCall {
    fn new(call_id: u64, timeout_ms: u64) -> Self {
        Self {
            call_id,
            response: None,
            completed: AtomicBool::new(false),
            timeout_ms,
        }
    }

    fn is_completed(&self) -> bool {
        self.completed.load(Ordering::Acquire)
    }

    fn complete(&mut self, response: IpiResponse) {
        self.response = Some(response);
        self.completed.store(true, Ordering::Release);
    }

    fn is_timed_out(&self) -> bool {
        get_time_msec() > self.timeout_ms
    }
}

/// Types of inter-processor interrupts
pub enum IpiMessage {
    /// Request target CPU to reschedule
    Reschedule {
        /// Optional sync call context
        sync_call: Option<u64>,
    },

    /// Request target CPU to flush TLB
    TlbFlush {
        /// Virtual address to flush (None for full flush)
        addr: Option<usize>,
        /// Address space ID (ASID)
        asid: Option<usize>,
        /// Optional sync call context
        sync_call: Option<u64>,
    },

    /// Execute a function on target CPU
    FunctionCall {
        /// Function to execute
        func: Box<dyn FnOnce() -> IpiResponse + Send>,
        /// Optional sync call context
        sync_call: Option<u64>,
    },

    /// Request target CPU to stop/halt
    Stop {
        /// Optional sync call context
        sync_call: Option<u64>,
    },

    /// Wake up target CPU from idle
    WakeUp {
        /// Optional sync call context
        sync_call: Option<u64>,
    },

    /// Generic message with data
    Generic {
        /// Message type identifier
        msg_type: usize,
        /// Message data
        data: usize,
        /// Optional sync call context
        sync_call: Option<u64>,
    },

    /// Synchronous response message
    SyncResponse {
        /// Call ID this response is for
        call_id: u64,
        /// Response data
        response: IpiResponse,
    },
}

impl IpiMessage {
    /// Get the priority of this message
    pub fn priority(&self) -> IpiPriority {
        match self {
            IpiMessage::Stop { .. } => IpiPriority::Critical,
            IpiMessage::TlbFlush { .. } => IpiPriority::High,
            IpiMessage::SyncResponse { .. } => IpiPriority::High,
            IpiMessage::Reschedule { .. } => IpiPriority::Normal,
            IpiMessage::FunctionCall { .. } => IpiPriority::Normal,
            IpiMessage::WakeUp { .. } => IpiPriority::Low,
            IpiMessage::Generic { .. } => IpiPriority::Low,
        }
    }

    /// Check if this is a synchronous message
    pub fn is_sync(&self) -> bool {
        match self {
            IpiMessage::Reschedule { sync_call } => sync_call.is_some(),
            IpiMessage::TlbFlush { sync_call, .. } => sync_call.is_some(),
            IpiMessage::FunctionCall { sync_call, .. } => sync_call.is_some(),
            IpiMessage::Stop { sync_call } => sync_call.is_some(),
            IpiMessage::WakeUp { sync_call } => sync_call.is_some(),
            IpiMessage::Generic { sync_call, .. } => sync_call.is_some(),
            IpiMessage::SyncResponse { .. } => false,
        }
    }

    /// Get sync call ID if this is a sync message
    pub fn sync_call_id(&self) -> Option<u64> {
        match self {
            IpiMessage::Reschedule { sync_call } => *sync_call,
            IpiMessage::TlbFlush { sync_call, .. } => *sync_call,
            IpiMessage::FunctionCall { sync_call, .. } => *sync_call,
            IpiMessage::Stop { sync_call } => *sync_call,
            IpiMessage::WakeUp { sync_call } => *sync_call,
            IpiMessage::Generic { sync_call, .. } => *sync_call,
            IpiMessage::SyncResponse { .. } => None,
        }
    }
}

impl core::fmt::Debug for IpiMessage {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            IpiMessage::Reschedule { sync_call } => {
                write!(f, "Reschedule {{ sync_call: {:?} }}", sync_call)
            }
            IpiMessage::TlbFlush { addr, asid, sync_call } => {
                write!(f, "TlbFlush {{ addr: {:?}, asid: {:?}, sync_call: {:?} }}", addr, asid, sync_call)
            }
            IpiMessage::FunctionCall { sync_call, .. } => {
                write!(f, "FunctionCall {{ func: <closure>, sync_call: {:?} }}", sync_call)
            }
            IpiMessage::Stop { sync_call } => {
                write!(f, "Stop {{ sync_call: {:?} }}", sync_call)
            }
            IpiMessage::WakeUp { sync_call } => {
                write!(f, "WakeUp {{ sync_call: {:?} }}", sync_call)
            }
            IpiMessage::Generic { msg_type, data, sync_call } => {
                write!(f, "Generic {{ msg_type: {}, data: {}, sync_call: {:?} }}", msg_type, data, sync_call)
            }
            IpiMessage::SyncResponse { call_id, response } => {
                write!(f, "SyncResponse {{ call_id: {}, response: {:?} }}", call_id, response)
            }
        }
    }
}

/// IPI statistics for monitoring
#[derive(Debug)]
pub struct IpiStats {
    /// Number of IPIs sent by this CPU
    pub sent: AtomicUsize,
    /// Number of IPIs received by this CPU
    pub received: AtomicUsize,
    /// Number of reschedule IPIs
    pub reschedule_count: AtomicUsize,
    /// Number of TLB flush IPIs
    pub tlb_flush_count: AtomicUsize,
    /// Number of function call IPIs
    pub function_call_count: AtomicUsize,
    /// Number of failed IPI sends
    pub send_failures: AtomicUsize,
}

impl IpiStats {
    pub fn new() -> Self {
        Self {
            sent: AtomicUsize::new(0),
            received: AtomicUsize::new(0),
            reschedule_count: AtomicUsize::new(0),
            tlb_flush_count: AtomicUsize::new(0),
            function_call_count: AtomicUsize::new(0),
            send_failures: AtomicUsize::new(0),
        }
    }
}

/// Priority-based IPI message queue entry
#[derive(Debug)]
struct PriorityIpiMessage {
    message: IpiMessage,
    priority: IpiPriority,
    timestamp: u64,
}

impl PriorityIpiMessage {
    fn new(message: IpiMessage) -> Self {
        let priority = message.priority();
        Self {
            message,
            priority,
            timestamp: get_time_msec(),
        }
    }
}

impl PartialEq for PriorityIpiMessage {
    fn eq(&self, other: &Self) -> bool {
        self.priority == other.priority && self.timestamp == other.timestamp
    }
}

impl Eq for PriorityIpiMessage {}

impl PartialOrd for PriorityIpiMessage {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for PriorityIpiMessage {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        // Higher priority (lower numeric value) comes first
        // If same priority, older timestamp comes first
        self.priority.cmp(&other.priority)
            .then(self.timestamp.cmp(&other.timestamp))
    }
}

/// Per-CPU IPI message queue with priority support
#[derive(Debug)]
struct IpiQueue {
    /// Priority-ordered queue of pending IPI messages
    messages: BTreeMap<IpiPriority, VecDeque<PriorityIpiMessage>>,
    /// Maximum queue size per priority to prevent memory exhaustion
    max_size_per_priority: usize,
    /// Number of dropped messages due to queue overflow
    dropped_count: usize,
    /// Total message count across all priorities
    total_count: usize,
}

impl IpiQueue {
    pub fn new() -> Self {
        Self {
            messages: BTreeMap::new(),
            max_size_per_priority: 16, // Reasonable default per priority
            dropped_count: 0,
            total_count: 0,
        }
    }

    /// Add a message to the queue with priority ordering
    pub fn push(&mut self, message: IpiMessage) -> Result<(), &'static str> {
        let priority_msg = PriorityIpiMessage::new(message);
        let priority = priority_msg.priority;

        // Check if this priority queue is full first
        let current_len = self.messages.get(&priority).map(|q| q.len()).unwrap_or(0);

        if current_len >= self.max_size_per_priority {
            // For critical messages, try to drop lower priority messages
            if priority == IpiPriority::Critical {
                if self.try_drop_lower_priority(priority) {
                    // Space was made, continue
                } else {
                    self.dropped_count += 1;
                    return Err("Critical IPI queue full");
                }
            } else {
                self.dropped_count += 1;
                return Err("IPI priority queue full");
            }
        }

        // Initialize priority queue if it doesn't exist and add message
        let priority_queue = self.messages.entry(priority).or_insert_with(VecDeque::new);
        priority_queue.push_back(priority_msg);
        self.total_count += 1;
        Ok(())
    }

    /// Try to drop lower priority messages to make space
    fn try_drop_lower_priority(&mut self, current_priority: IpiPriority) -> bool {
        // Try to drop from lowest priority queues first
        for priority in [IpiPriority::Low, IpiPriority::Normal, IpiPriority::High] {
            if priority < current_priority {
                if let Some(queue) = self.messages.get_mut(&priority) {
                    if !queue.is_empty() {
                        queue.pop_front();
                        self.total_count -= 1;
                        self.dropped_count += 1;
                        return true;
                    }
                }
            }
        }
        false
    }

    /// Remove and return the highest priority message
    pub fn pop(&mut self) -> Option<IpiMessage> {
        // Process in priority order: Critical -> High -> Normal -> Low
        for priority in [IpiPriority::Critical, IpiPriority::High, IpiPriority::Normal, IpiPriority::Low] {
            if let Some(queue) = self.messages.get_mut(&priority) {
                if let Some(priority_msg) = queue.pop_front() {
                    self.total_count -= 1;
                    return Some(priority_msg.message);
                }
            }
        }
        None
    }

    /// Check if queue is empty
    pub fn is_empty(&self) -> bool {
        self.total_count == 0
    }

    /// Get total queue length across all priorities
    pub fn len(&self) -> usize {
        self.total_count
    }

    /// Get queue length for a specific priority
    pub fn len_for_priority(&self, priority: IpiPriority) -> usize {
        self.messages.get(&priority).map(|q| q.len()).unwrap_or(0)
    }

    /// Get number of dropped messages
    pub fn dropped_count(&self) -> usize {
        self.dropped_count
    }

    /// Get maximum size per priority
    pub fn max_size_per_priority(&self) -> usize {
        self.max_size_per_priority
    }

    /// Set maximum size per priority
    pub fn set_max_size_per_priority(&mut self, size: usize) {
        self.max_size_per_priority = size;
    }

    /// Check if queue has messages with higher or equal priority
    pub fn has_priority_or_higher(&self, min_priority: IpiPriority) -> bool {
        for priority in [IpiPriority::Critical, IpiPriority::High, IpiPriority::Normal, IpiPriority::Low] {
            if priority <= min_priority {
                if let Some(queue) = self.messages.get(&priority) {
                    if !queue.is_empty() {
                        return true;
                    }
                }
            }
        }
        false
    }
}

/// IPI Barrier for CPU synchronization
#[derive(Debug)]
struct IpiBarrier {
    /// Barrier ID
    id: u64,
    /// Number of CPUs expected to participate
    expected_cpus: usize,
    /// CPUs that have arrived at the barrier
    arrived_cpus: AtomicUsize,
    /// CPUs that are waiting
    waiting_cpus: [AtomicBool; MAX_CPU_NUM],
    /// Barrier completion flag
    completed: AtomicBool,
    /// Timeout timestamp
    timeout_ms: u64,
}

impl IpiBarrier {
    fn new(id: u64, expected_cpus: usize, timeout_ms: u64) -> Self {
        const INIT_ATOMIC_BOOL: AtomicBool = AtomicBool::new(false);
        Self {
            id,
            expected_cpus,
            arrived_cpus: AtomicUsize::new(0),
            waiting_cpus: [INIT_ATOMIC_BOOL; MAX_CPU_NUM],
            completed: AtomicBool::new(false),
            timeout_ms,
        }
    }

    fn arrive(&self, cpu_id: usize) -> bool {
        if cpu_id >= MAX_CPU_NUM {
            return false;
        }

        self.waiting_cpus[cpu_id].store(true, Ordering::Release);
        let arrived = self.arrived_cpus.fetch_add(1, Ordering::AcqRel) + 1;

        if arrived >= self.expected_cpus {
            self.completed.store(true, Ordering::Release);
            return true;
        }
        false
    }

    fn is_completed(&self) -> bool {
        self.completed.load(Ordering::Acquire)
    }

    fn is_timed_out(&self) -> bool {
        get_time_msec() > self.timeout_ms
    }

    fn wait(&self, cpu_id: usize) -> Result<(), &'static str> {
        if cpu_id >= MAX_CPU_NUM {
            return Err("Invalid CPU ID");
        }

        // Wait for completion or timeout
        while !self.is_completed() && !self.is_timed_out() {
            core::hint::spin_loop();
        }

        if self.is_timed_out() {
            Err("Barrier timeout")
        } else {
            Ok(())
        }
    }
}

/// Global IPI management structure
struct IpiManager {
    /// Per-CPU message queues
    queues: [SpinLock<IpiQueue>; MAX_CPU_NUM],
    /// Per-CPU statistics
    stats: [IpiStats; MAX_CPU_NUM],
    /// Synchronous call tracking
    sync_calls: SpinLock<BTreeMap<u64, SpinLock<SyncIpiCall>>>,
    /// Next sync call ID
    next_sync_id: AtomicU64,
    /// IPI barrier states
    barriers: SpinLock<BTreeMap<u64, IpiBarrier>>,
    /// Next barrier ID
    next_barrier_id: AtomicU64,
}

impl IpiManager {
    const fn new() -> Self {
        const INIT_QUEUE: SpinLock<IpiQueue> = SpinLock::new(IpiQueue {
            messages: BTreeMap::new(),
            max_size_per_priority: 16,
            dropped_count: 0,
            total_count: 0,
        });
        const INIT_STATS: IpiStats = IpiStats {
            sent: AtomicUsize::new(0),
            received: AtomicUsize::new(0),
            reschedule_count: AtomicUsize::new(0),
            tlb_flush_count: AtomicUsize::new(0),
            function_call_count: AtomicUsize::new(0),
            send_failures: AtomicUsize::new(0),
        };

        Self {
            queues: [INIT_QUEUE; MAX_CPU_NUM],
            stats: [INIT_STATS; MAX_CPU_NUM],
            sync_calls: SpinLock::new(BTreeMap::new()),
            next_sync_id: AtomicU64::new(1),
            barriers: SpinLock::new(BTreeMap::new()),
            next_barrier_id: AtomicU64::new(1),
        }
    }

    /// Generate a new sync call ID
    fn next_sync_call_id(&self) -> u64 {
        self.next_sync_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Generate a new barrier ID
    fn next_barrier_id(&self) -> u64 {
        self.next_barrier_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Clean up expired sync calls and barriers
    fn cleanup_expired(&self) {
        let current_time = get_time_msec();

        // Clean up expired sync calls
        let mut sync_calls = self.sync_calls.lock();
        sync_calls.retain(|_, call_lock| {
            let call = call_lock.lock();
            !call.is_timed_out()
        });
        drop(sync_calls);

        // Clean up expired barriers
        let mut barriers = self.barriers.lock();
        barriers.retain(|_, barrier| {
            !barrier.is_timed_out()
        });
        drop(barriers);
    }
}

/// Global IPI manager instance
static IPI_MANAGER: IpiManager = IpiManager::new();

/// Send an IPI message to a specific CPU (asynchronous)
pub fn send_ipi(target_cpu: usize, message: IpiMessage) -> Result<(), &'static str> {
    send_ipi_with_retry(target_cpu, message, 3, 100)
}

/// Send an IPI message to a specific CPU with retry mechanism
pub fn send_ipi_with_retry(target_cpu: usize, message: IpiMessage, max_retries: usize, retry_delay_ms: u64) -> Result<(), &'static str> {
    if target_cpu >= MAX_CPU_NUM {
        return Err("Invalid CPU ID");
    }

    if !cpu_is_online(target_cpu) {
        return Err("Target CPU is not online");
    }

    let current_cpu = current_cpu_id();
    if target_cpu == current_cpu {
        // Execute locally instead of sending IPI
        handle_ipi_message(message);
        return Ok(());
    }

    // Try to add message to target CPU's queue first (only once)
    debug!("CPU{} adding IPI message to CPU{} queue", current_cpu, target_cpu);
    let result = {
        let mut queue = IPI_MANAGER.queues[target_cpu].lock();
        let old_len = queue.len();
        let result = queue.push(message);
        let new_len = queue.len();
        info!("CPU{} IPI queue for CPU{}: before={}, after={}, result={:?}",
              current_cpu, target_cpu, old_len, new_len, result);
        result
    };

    if result.is_err() {
        error!("CPU{} failed to add IPI message to CPU{} queue", current_cpu, target_cpu);
        IPI_MANAGER.stats[current_cpu].send_failures.fetch_add(1, Ordering::Relaxed);
        return result;
    } else {
        debug!("CPU{} successfully added IPI message to CPU{} queue", current_cpu, target_cpu);
    }

    // Retry only the hardware IPI sending part
    match send_hardware_ipi_with_retry(target_cpu, max_retries) {
        Ok(_) => {
            IPI_MANAGER.stats[current_cpu].sent.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
        Err(err) => {
            IPI_MANAGER.stats[current_cpu].send_failures.fetch_add(1, Ordering::Relaxed);
            Err(err)
        }
    }
}

/// Send a synchronous IPI message to a specific CPU
pub fn send_ipi_sync(target_cpu: usize, mut message: IpiMessage, timeout_ms: u64) -> Result<IpiResponse, &'static str> {
    if target_cpu >= MAX_CPU_NUM {
        return Err("Invalid CPU ID");
    }

    if !cpu_is_online(target_cpu) {
        return Err("Target CPU is not online");
    }

    let current_cpu = current_cpu_id();
    if target_cpu == current_cpu {
        // Execute locally and return result immediately
        let response = handle_ipi_message_sync(message);
        return Ok(response);
    }

    // Generate sync call ID and setup tracking
    let call_id = IPI_MANAGER.next_sync_call_id();
    let timeout_timestamp = get_time_msec() + timeout_ms;
    let sync_call = SyncIpiCall::new(call_id, timeout_timestamp);

    // Add to sync call tracking
    {
        let mut sync_calls = IPI_MANAGER.sync_calls.lock();
        sync_calls.insert(call_id, SpinLock::new(sync_call));
    }

    // Add sync call ID to message
    match &mut message {
        IpiMessage::Reschedule { sync_call } => *sync_call = Some(call_id),
        IpiMessage::TlbFlush { sync_call, .. } => *sync_call = Some(call_id),
        IpiMessage::FunctionCall { sync_call, .. } => *sync_call = Some(call_id),
        IpiMessage::Stop { sync_call } => *sync_call = Some(call_id),
        IpiMessage::WakeUp { sync_call } => *sync_call = Some(call_id),
        IpiMessage::Generic { sync_call, .. } => *sync_call = Some(call_id),
        IpiMessage::SyncResponse { .. } => {
            return Err("Cannot make sync response synchronous");
        }
    }

    // Send the message
    if let Err(err) = send_ipi_with_retry(target_cpu, message, 3, 50) {
        // Clean up sync call tracking
        let mut sync_calls = IPI_MANAGER.sync_calls.lock();
        sync_calls.remove(&call_id);
        return Err(err);
    }

    // Wait for response or timeout
    let start_time = get_time_msec();
    debug!("CPU{} waiting for sync response from CPU{}, call_id={}, timeout={}ms",
           current_cpu, target_cpu, call_id, timeout_ms);

    let mut loop_count = 0;
    loop {
        loop_count += 1;

        // Periodic debug output
        if loop_count % 10000 == 0 {
            let elapsed = get_time_msec() - start_time;
            debug!("CPU{} still waiting for sync response (elapsed={}ms, loops={})",
                   current_cpu, elapsed, loop_count);
        }

        // Check if call completed
        if let Some(call_lock) = IPI_MANAGER.sync_calls.lock().get(&call_id) {
            let call = call_lock.lock();
            if call.is_completed() {
                let response = call.response.as_ref().unwrap();
                debug!("CPU{} received sync response: {:?} (loops={})", current_cpu, response, loop_count);
                let result = match response {
                    IpiResponse::Success => Ok(IpiResponse::Success),
                    IpiResponse::Value(v) => Ok(IpiResponse::Value(*v)),
                    IpiResponse::Error(e) => Ok(IpiResponse::Error(e)),
                    IpiResponse::Timeout => Ok(IpiResponse::Timeout),
                };
                drop(call);

                // Clean up
                let mut sync_calls = IPI_MANAGER.sync_calls.lock();
                sync_calls.remove(&call_id);

                return result;
            }

            if call.is_timed_out() {
                let elapsed = get_time_msec() - start_time;
                warn!("CPU{} sync call {} timed out after {}ms (loops={})",
                      current_cpu, call_id, elapsed, loop_count);
                drop(call);

                // Clean up
                let mut sync_calls = IPI_MANAGER.sync_calls.lock();
                sync_calls.remove(&call_id);

                return Ok(IpiResponse::Timeout);
            }
        } else {
            error!("CPU{} sync call tracking lost for call_id={}", current_cpu, call_id);
            return Err("Sync call tracking lost");
        }

        // Small delay to avoid busy waiting
        core::hint::spin_loop();
    }
}

/// Send IPI to multiple CPUs
pub fn send_ipi_broadcast(message: IpiMessage, exclude_self: bool) -> Result<usize, &'static str> {
    let current_cpu = current_cpu_id();
    let mut success_count = 0;

    for cpu_id in 0..MAX_CPU_NUM {
        if !cpu_is_online(cpu_id) {
            continue;
        }

        if exclude_self && cpu_id == current_cpu {
            continue;
        }

        // Clone the message for each CPU (sync calls cannot be broadcasted)
        let cloned_message = match &message {
            IpiMessage::Reschedule { sync_call } => {
                if sync_call.is_some() {
                    continue; // Skip sync calls in broadcast
                }
                IpiMessage::Reschedule { sync_call: None }
            },
            IpiMessage::TlbFlush { addr, asid, sync_call } => {
                if sync_call.is_some() {
                    continue; // Skip sync calls in broadcast
                }
                IpiMessage::TlbFlush {
                    addr: *addr,
                    asid: *asid,
                    sync_call: None,
                }
            },
            IpiMessage::Stop { sync_call } => {
                if sync_call.is_some() {
                    continue; // Skip sync calls in broadcast
                }
                IpiMessage::Stop { sync_call: None }
            },
            IpiMessage::WakeUp { sync_call } => {
                if sync_call.is_some() {
                    continue; // Skip sync calls in broadcast
                }
                IpiMessage::WakeUp { sync_call: None }
            },
            IpiMessage::Generic { msg_type, data, sync_call } => {
                if sync_call.is_some() {
                    continue; // Skip sync calls in broadcast
                }
                IpiMessage::Generic {
                    msg_type: *msg_type,
                    data: *data,
                    sync_call: None,
                }
            },
            IpiMessage::FunctionCall { .. } => {
                // Function calls cannot be cloned, skip
                continue;
            },
            IpiMessage::SyncResponse { .. } => {
                // Sync responses are point-to-point, skip
                continue;
            }
        };

        if send_ipi(cpu_id, cloned_message).is_ok() {
            success_count += 1;
        }
    }

    Ok(success_count)
}

/// Handle incoming IPI interrupt
pub fn handle_ipi_interrupt() {
    let cpu_id = current_cpu_id();

    // Validate CPU ID to prevent array bounds violations
    if cpu_id >= MAX_CPU_NUM {
        error!("Invalid CPU ID {} in handle_ipi_interrupt", cpu_id);
        return;
    }

    // More frequent heartbeat for secondary CPUs to confirm they're checking for IPIs
    static LAST_IPI_CHECK: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
    if cpu_id > 0 {
        let current_time = get_time_msec();
        let last_check = LAST_IPI_CHECK.load(core::sync::atomic::Ordering::Relaxed);
        if current_time.saturating_sub(last_check) > 1000 { // Every 1 second
            LAST_IPI_CHECK.store(current_time, core::sync::atomic::Ordering::Relaxed);
            info!("CPU{} IPI heartbeat check, time={}ms", cpu_id, current_time);
        }
    }

    // Check if there are any pending messages with error handling
    let queue_len = match IPI_MANAGER.queues.get(cpu_id) {
        Some(queue) => {
            let locked_queue = queue.lock();
            let len = locked_queue.len();
            len
        },
        None => {
            error!("No IPI queue found for CPU {}", cpu_id);
            return;
        }
    };

    if queue_len > 0 {
        info!("CPU{} found {} pending IPI messages - PROCESSING", cpu_id, queue_len);
        if let Some(stats) = IPI_MANAGER.stats.get(cpu_id) {
            stats.received.fetch_add(1, Ordering::Relaxed);
        }
    }

    // Process all pending messages with bounds checking
    let mut message_count = 0;
    if let Some(queue) = IPI_MANAGER.queues.get(cpu_id) {
        while let Some(message) = queue.lock().pop() {
            message_count += 1;
            info!("CPU{} processing IPI message #{}: {:?}", cpu_id, message_count, message);
            handle_ipi_message(message);
        }
    }

    if message_count == 0 && queue_len > 0 {
        error!("CPU{} had {} queued messages but couldn't pop any", cpu_id, queue_len);
    } else if message_count > 0 {
        info!("CPU{} processed {} IPI messages - COMPLETE", cpu_id, message_count);
    }
}

/// Handle a specific IPI message (asynchronous)
fn handle_ipi_message(message: IpiMessage) {
    let response = handle_ipi_message_sync(message);

    // For async messages, we don't need to do anything with the response
    // unless it's an error that should be logged
    match response {
        IpiResponse::Error(err) => {
            error!("IPI message handling failed: {}", err);
        }
        _ => {} // Success cases don't need logging for async messages
    }
}

/// Handle a specific IPI message and return response (synchronous)
fn handle_ipi_message_sync(message: IpiMessage) -> IpiResponse {
    let cpu_id = current_cpu_id();

    match message {
        IpiMessage::Reschedule { sync_call } => {
            if let Some(stats) = IPI_MANAGER.stats.get(cpu_id) {
                stats.reschedule_count.fetch_add(1, Ordering::Relaxed);
            }
            handle_reschedule_ipi();
            let response = IpiResponse::Success;

            if let Some(call_id) = sync_call {
                send_sync_response(call_id, response.clone());
            }
            response
        }

        IpiMessage::TlbFlush { addr, asid, sync_call } => {
            if let Some(stats) = IPI_MANAGER.stats.get(cpu_id) {
                stats.tlb_flush_count.fetch_add(1, Ordering::Relaxed);
            }
            TlbManager::flush_local(addr);
            let response = IpiResponse::Success;

            if let Some(call_id) = sync_call {
                send_sync_response(call_id, response.clone());
            }
            response
        }

        IpiMessage::FunctionCall { func, sync_call } => {
            if let Some(stats) = IPI_MANAGER.stats.get(cpu_id) {
                stats.function_call_count.fetch_add(1, Ordering::Relaxed);
            }
            let response = func();

            if let Some(call_id) = sync_call {
                send_sync_response(call_id, response.clone());
            }
            response
        }

        IpiMessage::Stop { sync_call } => {
            let response = IpiResponse::Success;

            if let Some(call_id) = sync_call {
                send_sync_response(call_id, response.clone());
            }

            handle_stop_ipi();
            response
        }

        IpiMessage::WakeUp { sync_call } => {
            handle_wakeup_ipi();
            let response = IpiResponse::Success;

            if let Some(call_id) = sync_call {
                send_sync_response(call_id, response.clone());
            }
            response
        }

        IpiMessage::Generic { msg_type, data, sync_call } => {
            let response = handle_generic_ipi_sync(msg_type, data);

            if let Some(call_id) = sync_call {
                send_sync_response(call_id, response.clone());
            }
            response
        }

        IpiMessage::SyncResponse { call_id, response } => {
            handle_sync_response(call_id, response.clone());
            response
        }
    }
}

/// Send synchronous response back to caller
fn send_sync_response(call_id: u64, response: IpiResponse) {
    let cpu_id = current_cpu_id();
    debug!("CPU{} sending sync response for call_id={}", cpu_id, call_id);

    // Find the original caller by looking up the sync call
    let sync_calls = IPI_MANAGER.sync_calls.lock();
    if let Some(call_lock) = sync_calls.get(&call_id) {
        let mut call = call_lock.lock();
        call.complete(response);
        debug!("CPU{} successfully completed sync call {}", cpu_id, call_id);
    } else {
        // This is not necessarily an error during boot phase
        debug!("CPU{} could not find sync call {} to complete (may be cleaned up)", cpu_id, call_id);
    }
}

/// Handle synchronous response from remote CPU
fn handle_sync_response(call_id: u64, response: IpiResponse) {
    if let Some(call_lock) = IPI_MANAGER.sync_calls.lock().get(&call_id) {
        let mut call = call_lock.lock();
        call.complete(response);
    }
}

/// Send hardware IPI to target CPU
fn send_hardware_ipi(target_cpu: usize) -> Result<(), &'static str> {
    send_hardware_ipi_with_retry(target_cpu, 1)
}

/// Send hardware IPI to target CPU with retry mechanism
fn send_hardware_ipi_with_retry(target_cpu: usize, max_retries: usize) -> Result<(), &'static str> {
    for attempt in 0..=max_retries {
        let result = try_send_hardware_ipi(target_cpu);

        if result.is_ok() {
            return Ok(());
        }

        if attempt < max_retries {
            // Short delay before retry
            for _ in 0..1000 {
                core::hint::spin_loop();
            }
        }
    }

    Err("Hardware IPI send failed after retries")
}

/// Try to send hardware IPI once
fn try_send_hardware_ipi(target_cpu: usize) -> Result<(), &'static str> {
    // For RISC-V, we use SBI to send IPIs
    #[cfg(target_arch = "riscv64")]
    {
        // Get the hart ID for the target CPU
        if let Some(cpu_data) = crate::smp::cpu_data(target_cpu) {
            let hart_id = cpu_data.arch_cpu_id.load(Ordering::Relaxed);
            let hart_mask = 1 << hart_id;
            debug!("Sending IPI to CPU{} (hart_id={}, hart_mask={:#x})", target_cpu, hart_id, hart_mask);
            match sbi::send_ipi(hart_mask) {
                Ok(()) => {
                    debug!("SBI IPI send successful: CPU{} hart_id={} hart_mask={:#x}",
                           target_cpu, hart_id, hart_mask);
                    Ok(())
                }
                Err(e) => {
                    error!("SBI IPI send failed: CPU{} hart_id={} hart_mask={:#x} error={}",
                           target_cpu, hart_id, hart_mask, e);
                    Err("SBI IPI send failed")
                }
            }
        } else {
            error!("Cannot find CPU data for target CPU{}", target_cpu);
            Err("Cannot find CPU data for target CPU")
        }
    }

    #[cfg(not(target_arch = "riscv64"))]
    {
        // Placeholder for other architectures
        warn!("Hardware IPI not implemented for this architecture");
        Ok(())
    }
}

/// Handle reschedule IPI with enhanced task discovery
fn handle_reschedule_ipi() {
    let cpu_id = current_cpu_id();

    if let Some(cpu_data) = crate::smp::current_cpu_data() {
        // Set the reschedule flag for the current CPU
        cpu_data.set_need_resched(true);

        debug!("CPU{} received reschedule IPI, queue_len={}, current_task={}",
               cpu_id,
               cpu_data.queue_length(),
               cpu_data.current_task().map(|t| t.pid()).unwrap_or(0));

        // If CPU is idle, wake it up immediately
        if cpu_data.state() == crate::smp::cpu::CpuState::Idle {
            debug!("CPU{} waking up from idle due to reschedule IPI", cpu_id);
            cpu_data.set_state(crate::smp::cpu::CpuState::Online);
        }

        // If not in interrupt context and we have a current task, consider preemption
        if !cpu_data.in_interrupt() {
            if let Some(current_task) = cpu_data.current_task() {
                // Check if we have higher priority tasks waiting
                if cpu_data.queue_length() > 0 {
                    debug!("CPU{} preempting current task {} due to waiting tasks",
                           cpu_id, current_task.pid());
                    crate::task::suspend_current_and_run_next();
                }
            } else if cpu_data.queue_length() > 0 {
                // No current task but we have tasks waiting - this should trigger scheduling
                debug!("CPU{} has no current task but {} tasks waiting", cpu_id, cpu_data.queue_length());
            }
        } else {
            debug!("CPU{} received reschedule IPI in interrupt context, deferring", cpu_id);
        }
    } else {
        error!("No CPU data available when handling reschedule IPI on CPU {}", cpu_id);
    }
}

/// Handle stop IPI
fn handle_stop_ipi() {
    let cpu_id = current_cpu_id();
    info!("CPU {} received stop request", cpu_id);

    if let Some(cpu_data) = crate::smp::current_cpu_data() {
        cpu_data.set_state(crate::smp::cpu::CpuState::Stopping);
    }

    // Disable interrupts and halt
    #[cfg(target_arch = "riscv64")]
    unsafe {
        riscv::interrupt::disable();
        loop {
            riscv::asm::wfi();
        }
    }
}

/// Handle wakeup IPI
fn handle_wakeup_ipi() {
    // Just acknowledge the wakeup - the CPU is already awake if we're handling this
    debug!("CPU {} woken up", current_cpu_id());
}

/// Handle generic IPI (asynchronous version)
fn handle_generic_ipi(msg_type: usize, data: usize) {
    let _response = handle_generic_ipi_sync(msg_type, data);
}

/// Handle generic IPI and return response (synchronous version)
fn handle_generic_ipi_sync(msg_type: usize, data: usize) -> IpiResponse {
    debug!("CPU {} received generic IPI: type={}, data={:#x}",
           current_cpu_id(), msg_type, data);

    // Application-specific handling can be added here
    // For now, just return success
    IpiResponse::Success
}

/// Convenience functions for common IPI operations

/// Send reschedule IPI to a specific CPU (asynchronous)
pub fn send_reschedule_ipi(target_cpu: usize) -> Result<(), &'static str> {
    send_ipi(target_cpu, IpiMessage::Reschedule { sync_call: None })
}

/// Send reschedule IPI to a specific CPU (synchronous)
pub fn send_reschedule_ipi_sync(target_cpu: usize, timeout_ms: u64) -> Result<IpiResponse, &'static str> {
    send_ipi_sync(target_cpu, IpiMessage::Reschedule { sync_call: None }, timeout_ms)
}

/// Send reschedule IPI to all other CPUs
pub fn send_reschedule_ipi_broadcast() -> Result<usize, &'static str> {
    send_ipi_broadcast(IpiMessage::Reschedule { sync_call: None }, true)
}

/// Send TLB flush IPI to a specific CPU (asynchronous)
pub fn send_tlb_flush_ipi(target_cpu: usize, addr: Option<usize>) -> Result<(), &'static str> {
    send_ipi(target_cpu, IpiMessage::TlbFlush {
        addr,
        asid: None,
        sync_call: None
    })
}

/// Send TLB flush IPI to a specific CPU (synchronous)
pub fn send_tlb_flush_ipi_sync(target_cpu: usize, addr: Option<usize>, timeout_ms: u64) -> Result<IpiResponse, &'static str> {
    send_ipi_sync(target_cpu, IpiMessage::TlbFlush {
        addr,
        asid: None,
        sync_call: None
    }, timeout_ms)
}

/// Send TLB flush IPI to all CPUs
pub fn send_tlb_flush_ipi_broadcast(addr: Option<usize>) -> Result<usize, &'static str> {
    send_ipi_broadcast(IpiMessage::TlbFlush {
        addr,
        asid: None,
        sync_call: None
    }, false)
}

/// Execute a function on a specific CPU via IPI (asynchronous)
pub fn send_function_call_ipi<F>(target_cpu: usize, func: F) -> Result<(), &'static str>
where
    F: FnOnce() -> IpiResponse + Send + 'static,
{
    send_ipi(target_cpu, IpiMessage::FunctionCall {
        func: Box::new(func),
        sync_call: None,
    })
}

/// Execute a function on a specific CPU via IPI (synchronous)
pub fn send_function_call_ipi_sync<F>(target_cpu: usize, func: F, timeout_ms: u64) -> Result<IpiResponse, &'static str>
where
    F: FnOnce() -> IpiResponse + Send + 'static,
{
    send_ipi_sync(target_cpu, IpiMessage::FunctionCall {
        func: Box::new(func),
        sync_call: None,
    }, timeout_ms)
}

/// Send stop IPI to a specific CPU (asynchronous)
pub fn send_stop_ipi(target_cpu: usize) -> Result<(), &'static str> {
    send_ipi(target_cpu, IpiMessage::Stop { sync_call: None })
}

/// Send stop IPI to a specific CPU (synchronous)
pub fn send_stop_ipi_sync(target_cpu: usize, timeout_ms: u64) -> Result<IpiResponse, &'static str> {
    send_ipi_sync(target_cpu, IpiMessage::Stop { sync_call: None }, timeout_ms)
}

/// Send stop IPI to all other CPUs
pub fn send_stop_ipi_broadcast() -> Result<usize, &'static str> {
    send_ipi_broadcast(IpiMessage::Stop { sync_call: None }, true)
}

/// Wake up a specific CPU (asynchronous)
pub fn send_wakeup_ipi(target_cpu: usize) -> Result<(), &'static str> {
    send_ipi(target_cpu, IpiMessage::WakeUp { sync_call: None })
}

/// Wake up a specific CPU (synchronous)
pub fn send_wakeup_ipi_sync(target_cpu: usize, timeout_ms: u64) -> Result<IpiResponse, &'static str> {
    send_ipi_sync(target_cpu, IpiMessage::WakeUp { sync_call: None }, timeout_ms)
}

/// Create an IPI barrier for CPU synchronization
pub fn create_ipi_barrier(cpu_mask: &[usize], timeout_ms: u64) -> Result<u64, &'static str> {
    if cpu_mask.is_empty() {
        return Err("Empty CPU mask");
    }

    // Validate CPU IDs
    for &cpu_id in cpu_mask {
        if cpu_id >= MAX_CPU_NUM {
            return Err("Invalid CPU ID in mask");
        }
        if !cpu_is_online(cpu_id) {
            return Err("Offline CPU in mask");
        }
    }

    let barrier_id = IPI_MANAGER.next_barrier_id();
    let timeout_timestamp = get_time_msec() + timeout_ms;
    let barrier = IpiBarrier::new(barrier_id, cpu_mask.len(), timeout_timestamp);

    // Add barrier to tracking
    {
        let mut barriers = IPI_MANAGER.barriers.lock();
        barriers.insert(barrier_id, barrier);
    }

    Ok(barrier_id)
}

/// Wait at an IPI barrier
pub fn wait_at_ipi_barrier(barrier_id: u64) -> Result<(), &'static str> {
    let cpu_id = current_cpu_id();

    // Find the barrier
    let barrier_completed = {
        let barriers = IPI_MANAGER.barriers.lock();
        if let Some(barrier) = barriers.get(&barrier_id) {
            // Arrive at the barrier
            barrier.arrive(cpu_id);

            // Wait for completion
            barrier.wait(cpu_id)?;
            barrier.is_completed()
        } else {
            return Err("Barrier not found");
        }
    };

    if barrier_completed {
        // Clean up barrier if we're the last one
        let mut barriers = IPI_MANAGER.barriers.lock();
        barriers.remove(&barrier_id);
        Ok(())
    } else {
        Err("Barrier failed")
    }
}

/// Check if a barrier is completed
pub fn is_ipi_barrier_completed(barrier_id: u64) -> bool {
    let barriers = IPI_MANAGER.barriers.lock();
    if let Some(barrier) = barriers.get(&barrier_id) {
        barrier.is_completed()
    } else {
        false
    }
}

/// Destroy an IPI barrier (cleanup)
pub fn destroy_ipi_barrier(barrier_id: u64) -> Result<(), &'static str> {
    let mut barriers = IPI_MANAGER.barriers.lock();
    if barriers.remove(&barrier_id).is_some() {
        Ok(())
    } else {
        Err("Barrier not found")
    }
}

/// Periodic cleanup of expired sync calls and barriers
pub fn cleanup_expired_ipi_resources() {
    IPI_MANAGER.cleanup_expired();
}

/// Get IPI statistics for a specific CPU
pub fn get_ipi_stats(cpu_id: usize) -> Option<&'static IpiStats> {
    if cpu_id < MAX_CPU_NUM {
        Some(&IPI_MANAGER.stats[cpu_id])
    } else {
        None
    }
}

/// Get current CPU's IPI statistics
pub fn current_ipi_stats() -> Option<&'static IpiStats> {
    get_ipi_stats(current_cpu_id())
}

/// Check if IPI queue is full for a CPU (any priority)
pub fn is_ipi_queue_full(cpu_id: usize) -> bool {
    if cpu_id < MAX_CPU_NUM {
        let queue = IPI_MANAGER.queues[cpu_id].lock();
        // Check if all priority queues are at capacity
        for priority in [IpiPriority::Critical, IpiPriority::High, IpiPriority::Normal, IpiPriority::Low] {
            if queue.len_for_priority(priority) < queue.max_size_per_priority() {
                return false;
            }
        }
        true
    } else {
        false
    }
}

/// Check if IPI queue is full for a specific priority
pub fn is_ipi_queue_full_for_priority(cpu_id: usize, priority: IpiPriority) -> bool {
    if cpu_id < MAX_CPU_NUM {
        let queue = IPI_MANAGER.queues[cpu_id].lock();
        queue.len_for_priority(priority) >= queue.max_size_per_priority()
    } else {
        false
    }
}

/// Get IPI queue status for debugging
pub fn get_ipi_queue_status(cpu_id: usize) -> Option<(usize, usize, usize)> {
    if cpu_id < MAX_CPU_NUM {
        let queue = IPI_MANAGER.queues[cpu_id].lock();
        Some((queue.len(), queue.max_size_per_priority() * 4, queue.dropped_count()))
    } else {
        None
    }
}

/// Get detailed IPI queue status by priority
pub fn get_ipi_queue_status_detailed(cpu_id: usize) -> Option<[(usize, usize); 4]> {
    if cpu_id < MAX_CPU_NUM {
        let queue = IPI_MANAGER.queues[cpu_id].lock();
        let max_size = queue.max_size_per_priority();
        Some([
            (queue.len_for_priority(IpiPriority::Critical), max_size),
            (queue.len_for_priority(IpiPriority::High), max_size),
            (queue.len_for_priority(IpiPriority::Normal), max_size),
            (queue.len_for_priority(IpiPriority::Low), max_size),
        ])
    } else {
        None
    }
}