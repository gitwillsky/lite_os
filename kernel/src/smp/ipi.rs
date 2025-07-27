/// Inter-Processor Interrupt (IPI) support for SMP systems
///
/// This module provides the infrastructure for CPU-to-CPU communication
/// through interrupts, enabling coordination between processors.

use alloc::{boxed::Box, collections::VecDeque, sync::Arc};
use core::sync::atomic::{AtomicUsize, Ordering};
use crate::{
    sync::spinlock::SpinLock,
    smp::{MAX_CPU_NUM, current_cpu_id, cpu_is_online},
    arch::sbi,
    memory::TlbManager,
};

/// Types of inter-processor interrupts
pub enum IpiMessage {
    /// Request target CPU to reschedule
    Reschedule,

    /// Request target CPU to flush TLB
    TlbFlush {
        /// Virtual address to flush (None for full flush)
        addr: Option<usize>,
        /// Address space ID (ASID)
        asid: Option<usize>,
    },

    /// Execute a function on target CPU
    FunctionCall {
        /// Function to execute
        func: Box<dyn FnOnce() + Send>,
    },

    /// Request target CPU to stop/halt
    Stop,

    /// Wake up target CPU from idle
    WakeUp,

    /// Generic message with data
    Generic {
        /// Message type identifier
        msg_type: usize,
        /// Message data
        data: usize,
    },
}

impl core::fmt::Debug for IpiMessage {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            IpiMessage::Reschedule => write!(f, "Reschedule"),
            IpiMessage::TlbFlush { addr, asid } => {
                write!(f, "TlbFlush {{ addr: {:?}, asid: {:?} }}", addr, asid)
            }
            IpiMessage::FunctionCall { .. } => write!(f, "FunctionCall {{ func: <closure> }}"),
            IpiMessage::Stop => write!(f, "Stop"),
            IpiMessage::WakeUp => write!(f, "WakeUp"),
            IpiMessage::Generic { msg_type, data } => {
                write!(f, "Generic {{ msg_type: {}, data: {} }}", msg_type, data)
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

/// Per-CPU IPI message queue
#[derive(Debug)]
struct IpiQueue {
    /// Queue of pending IPI messages
    messages: VecDeque<IpiMessage>,
    /// Maximum queue size to prevent memory exhaustion
    max_size: usize,
    /// Number of dropped messages due to queue overflow
    dropped_count: usize,
}

impl IpiQueue {
    pub fn new() -> Self {
        Self {
            messages: VecDeque::new(),
            max_size: 64, // Reasonable default
            dropped_count: 0,
        }
    }

    /// Add a message to the queue
    pub fn push(&mut self, message: IpiMessage) -> Result<(), &'static str> {
        if self.messages.len() >= self.max_size {
            self.dropped_count += 1;
            return Err("IPI queue full");
        }

        self.messages.push_back(message);
        Ok(())
    }

    /// Remove and return the next message
    pub fn pop(&mut self) -> Option<IpiMessage> {
        self.messages.pop_front()
    }

    /// Check if queue is empty
    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    /// Get queue length
    pub fn len(&self) -> usize {
        self.messages.len()
    }

    /// Get number of dropped messages
    pub fn dropped_count(&self) -> usize {
        self.dropped_count
    }
}

/// Global IPI management structure
struct IpiManager {
    /// Per-CPU message queues
    queues: [SpinLock<IpiQueue>; MAX_CPU_NUM],
    /// Per-CPU statistics
    stats: [IpiStats; MAX_CPU_NUM],
}

impl IpiManager {
    const fn new() -> Self {
        const INIT_QUEUE: SpinLock<IpiQueue> = SpinLock::new(IpiQueue {
            messages: VecDeque::new(),
            max_size: 64,
            dropped_count: 0,
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
        }
    }
}

/// Global IPI manager instance
static IPI_MANAGER: IpiManager = IpiManager::new();

/// Initialize IPI subsystem
pub fn init() {
    info!("Inter-processor interrupt subsystem initialized");
}

/// Send an IPI message to a specific CPU
pub fn send_ipi(target_cpu: usize, message: IpiMessage) -> Result<(), &'static str> {
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

    // Add message to target CPU's queue
    let result = IPI_MANAGER.queues[target_cpu].lock().push(message);

    if result.is_ok() {
        // Send the actual interrupt to the target CPU
        if let Err(_) = send_hardware_ipi(target_cpu) {
            IPI_MANAGER.stats[current_cpu].send_failures.fetch_add(1, Ordering::Relaxed);
            return Err("Failed to send hardware IPI");
        }

        IPI_MANAGER.stats[current_cpu].sent.fetch_add(1, Ordering::Relaxed);
        Ok(())
    } else {
        IPI_MANAGER.stats[current_cpu].send_failures.fetch_add(1, Ordering::Relaxed);
        result
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

        // Clone the message for each CPU
        let cloned_message = match &message {
            IpiMessage::Reschedule => IpiMessage::Reschedule,
            IpiMessage::TlbFlush { addr, asid } => IpiMessage::TlbFlush {
                addr: *addr,
                asid: *asid
            },
            IpiMessage::Stop => IpiMessage::Stop,
            IpiMessage::WakeUp => IpiMessage::WakeUp,
            IpiMessage::Generic { msg_type, data } => IpiMessage::Generic {
                msg_type: *msg_type,
                data: *data
            },
            IpiMessage::FunctionCall { .. } => {
                // Function calls cannot be cloned, skip
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
    IPI_MANAGER.stats[cpu_id].received.fetch_add(1, Ordering::Relaxed);

    // Process all pending messages
    while let Some(message) = IPI_MANAGER.queues[cpu_id].lock().pop() {
        handle_ipi_message(message);
    }
}

/// Handle a specific IPI message
fn handle_ipi_message(message: IpiMessage) {
    let cpu_id = current_cpu_id();

    match message {
        IpiMessage::Reschedule => {
            IPI_MANAGER.stats[cpu_id].reschedule_count.fetch_add(1, Ordering::Relaxed);
            handle_reschedule_ipi();
        }

        IpiMessage::TlbFlush { addr, asid } => {
            IPI_MANAGER.stats[cpu_id].tlb_flush_count.fetch_add(1, Ordering::Relaxed);
            TlbManager::flush_local(addr);
        }

        IpiMessage::FunctionCall { func } => {
            IPI_MANAGER.stats[cpu_id].function_call_count.fetch_add(1, Ordering::Relaxed);
            func();
        }

        IpiMessage::Stop => {
            handle_stop_ipi();
        }

        IpiMessage::WakeUp => {
            handle_wakeup_ipi();
        }

        IpiMessage::Generic { msg_type, data } => {
            handle_generic_ipi(msg_type, data);
        }
    }
}

/// Send hardware IPI to target CPU
fn send_hardware_ipi(target_cpu: usize) -> Result<(), &'static str> {
    // For RISC-V, we use SBI to send IPIs
    #[cfg(target_arch = "riscv64")]
    {
        // Get the hart ID for the target CPU
        if let Some(cpu_data) = crate::smp::cpu_data(target_cpu) {
            let hart_mask = 1 << cpu_data.arch_cpu_id.load(Ordering::Relaxed);
            sbi::send_ipi(hart_mask).map_err(|_| "SBI IPI send failed")
        } else {
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

/// Handle reschedule IPI
fn handle_reschedule_ipi() {
    // Set the reschedule flag for the current CPU
    if let Some(cpu_data) = crate::smp::current_cpu_data() {
        cpu_data.set_need_resched(true);
    }

    // If not in interrupt context, immediately reschedule
    if let Some(cpu_data) = crate::smp::current_cpu_data() {
        if !cpu_data.in_interrupt() {
            crate::task::suspend_current_and_run_next();
        }
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

/// Handle generic IPI
fn handle_generic_ipi(msg_type: usize, data: usize) {
    debug!("CPU {} received generic IPI: type={}, data={:#x}",
           current_cpu_id(), msg_type, data);
    // Application-specific handling can be added here
}

/// Convenience functions for common IPI operations

/// Send reschedule IPI to a specific CPU
pub fn send_reschedule_ipi(target_cpu: usize) -> Result<(), &'static str> {
    send_ipi(target_cpu, IpiMessage::Reschedule)
}

/// Send reschedule IPI to all other CPUs
pub fn send_reschedule_ipi_broadcast() -> Result<usize, &'static str> {
    send_ipi_broadcast(IpiMessage::Reschedule, true)
}

/// Send TLB flush IPI to a specific CPU
pub fn send_tlb_flush_ipi(target_cpu: usize, addr: Option<usize>) -> Result<(), &'static str> {
    send_ipi(target_cpu, IpiMessage::TlbFlush { addr, asid: None })
}

/// Send TLB flush IPI to all CPUs
pub fn send_tlb_flush_ipi_broadcast(addr: Option<usize>) -> Result<usize, &'static str> {
    send_ipi_broadcast(IpiMessage::TlbFlush { addr, asid: None }, false)
}

/// Execute a function on a specific CPU via IPI
pub fn send_function_call_ipi<F>(target_cpu: usize, func: F) -> Result<(), &'static str>
where
    F: FnOnce() + Send + 'static,
{
    send_ipi(target_cpu, IpiMessage::FunctionCall {
        func: Box::new(func)
    })
}

/// Send stop IPI to a specific CPU
pub fn send_stop_ipi(target_cpu: usize) -> Result<(), &'static str> {
    send_ipi(target_cpu, IpiMessage::Stop)
}

/// Send stop IPI to all other CPUs
pub fn send_stop_ipi_broadcast() -> Result<usize, &'static str> {
    send_ipi_broadcast(IpiMessage::Stop, true)
}

/// Wake up a specific CPU
pub fn send_wakeup_ipi(target_cpu: usize) -> Result<(), &'static str> {
    send_ipi(target_cpu, IpiMessage::WakeUp)
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

/// Check if IPI queue is full for a CPU
pub fn is_ipi_queue_full(cpu_id: usize) -> bool {
    if cpu_id < MAX_CPU_NUM {
        let queue = IPI_MANAGER.queues[cpu_id].lock();
        queue.len() >= queue.max_size
    } else {
        false
    }
}

/// Get IPI queue status for debugging
pub fn get_ipi_queue_status(cpu_id: usize) -> Option<(usize, usize, usize)> {
    if cpu_id < MAX_CPU_NUM {
        let queue = IPI_MANAGER.queues[cpu_id].lock();
        Some((queue.len(), queue.max_size, queue.dropped_count()))
    } else {
        None
    }
}