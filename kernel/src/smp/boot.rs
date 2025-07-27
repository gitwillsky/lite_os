/// SMP boot sequence implementation
///
/// This module handles the multi-core boot process, including secondary
/// CPU initialization and synchronization between cores.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicUsize, AtomicBool, Ordering};
use crate::{
    smp::{
        cpu::{CpuData, CpuState, CpuType},
        current_cpu_id, set_cpu_data, cpu_set_online, cpu_data,
        MAX_CPU_NUM, init_cpu_id_register
    },
    arch::sbi,
    memory::{address::PhysicalAddress, KERNEL_SPACE, TlbManager},
    sync::spinlock::SpinLock,
};

static DTB_ADDR: AtomicUsize = AtomicUsize::new(0);

/// Boot synchronization barriers
static SECONDARY_CPU_READY_COUNT: AtomicUsize = AtomicUsize::new(0);
static PRIMARY_CPU_READY: AtomicBool = AtomicBool::new(false);
static BOOT_BARRIER: SpinLock<()> = SpinLock::new(());

/// Maximum time to wait for secondary CPUs to boot (in milliseconds)
const SECONDARY_CPU_BOOT_TIMEOUT_MS: u64 = 5000;

/// Entry point for secondary CPUs
/// This function is called by secondary CPUs after they are started by SBI
#[unsafe(no_mangle)]
pub extern "C" fn secondary_cpu_main(hart_id: usize, dtb_addr: usize) -> ! {
    // Initialize CPU ID register for this hart
    init_cpu_id_register(hart_id);

    // Convert hart ID to logical CPU ID
    let cpu_id = if let Some(logical_id) = crate::smp::topology::arch_to_logical_cpu_id(hart_id) {
        logical_id
    } else {
        // If not found in topology, use hart_id as fallback
        warn!("Unknown hart ID {}, using as logical CPU ID", hart_id);
        hart_id
    };

    info!("Secondary CPU {} (hart {}) starting initialization", cpu_id, hart_id);

    // Wait for primary CPU to complete global initialization
    while !PRIMARY_CPU_READY.load(Ordering::Acquire) {
        core::hint::spin_loop();
    }

    // Initialize this CPU
    secondary_cpu_init(cpu_id, hart_id);

    // Mark this CPU as ready
    SECONDARY_CPU_READY_COUNT.fetch_add(1, Ordering::AcqRel);

    info!("Secondary CPU {} initialization complete", cpu_id);

    // Enter the per-CPU scheduler loop
    secondary_cpu_scheduler_loop(cpu_id)
}

/// Initialize a secondary CPU
fn secondary_cpu_init(cpu_id: usize, hart_id: usize) {
    // Set CPU state to starting
    if let Some(cpu_data) = cpu_data(cpu_id) {
        cpu_data.set_state(CpuState::Starting);
    }

    // Initialize architecture-specific features
    arch_specific_secondary_init(hart_id);

    // Initialize per-CPU memory management
    secondary_cpu_memory_init(cpu_id);

    // Initialize per-CPU timer
    secondary_cpu_timer_init(cpu_id);

    // Mark CPU as online
    cpu_set_online(cpu_id);

    if let Some(cpu_data) = cpu_data(cpu_id) {
        cpu_data.set_state(CpuState::Online);
    }
}

/// Architecture-specific secondary CPU initialization
fn arch_specific_secondary_init(hart_id: usize) {
    #[cfg(target_arch = "riscv64")]
    {
        // Enable supervisor interrupts
        unsafe {
            riscv::register::sstatus::set_sie();

            // Set up interrupt delegation
            riscv::register::sie::set_sext();
            riscv::register::sie::set_stimer();
            riscv::register::sie::set_ssoft();
        }

        // Initialize floating point if available
        #[cfg(feature = "f")]
        unsafe {
            riscv::register::sstatus::set_fs(riscv::register::sstatus::FS::Initial);
        }

        debug!("Architecture-specific init complete for hart {}", hart_id);
    }
}

/// Initialize memory management for secondary CPU
fn secondary_cpu_memory_init(cpu_id: usize) {
    // Activate the kernel page table
    let kernel_space_arc = KERNEL_SPACE.wait();
    let kernel_space = kernel_space_arc.read();
    unsafe {
        // Set the page table for this CPU
        let token = kernel_space.token();
        #[cfg(target_arch = "riscv64")]
        riscv::register::satp::write(unsafe { core::mem::transmute(token) });

        // Flush TLB
        TlbManager::flush_local(None);
    }

    // Initialize per-CPU heap allocator if needed
    if let Some(cpu_data) = cpu_data(cpu_id) {
        // Per-CPU allocator will be initialized on first use
        debug!("Memory management initialized for CPU {}", cpu_id);
    }
}

/// Initialize timer for secondary CPU
fn secondary_cpu_timer_init(cpu_id: usize) {
    // Timer initialization is handled by the timer module
    crate::timer::init_secondary_cpu(cpu_id);
    debug!("Timer initialized for CPU {}", cpu_id);
}

/// Secondary CPU scheduler loop
fn secondary_cpu_scheduler_loop(cpu_id: usize) -> ! {
    debug!("CPU {} entering scheduler loop", cpu_id);

    loop {
        // Check if this CPU needs to be stopped
        if let Some(cpu_data) = cpu_data(cpu_id) {
            match cpu_data.state() {
                CpuState::Stopping => {
                    info!("CPU {} stopping", cpu_id);
                    secondary_cpu_stop(cpu_id);
                }
                CpuState::Error => {
                    error!("CPU {} in error state, halting", cpu_id);
                    secondary_cpu_halt(cpu_id);
                }
                _ => {}
            }
        }

        // Try to get a task from the local queue
        if let Some(cpu_data) = cpu_data(cpu_id) {
            if let Some(task) = cpu_data.pop_task() {
                // Execute the task
                execute_task_on_cpu(cpu_id, task);
                continue;
            }
        }

        // No local task, try work stealing
        if let Some(stolen_task) = try_steal_task(cpu_id) {
            execute_task_on_cpu(cpu_id, stolen_task);
            continue;
        }

        // No work available, enter idle state
        secondary_cpu_idle(cpu_id);
    }
}

/// Execute a task on the specified CPU
fn execute_task_on_cpu(cpu_id: usize, task: Arc<crate::task::TaskControlBlock>) {
    if let Some(cpu_data) = cpu_data(cpu_id) {
        // Set the current task
        cpu_data.set_current_task(Some(task.clone()));

        // Update task state
        *task.task_status.lock() = crate::task::TaskStatus::Running;

        // Record start time
        let start_time = crate::timer::get_time_us();
        task.last_runtime.store(start_time, Ordering::Relaxed);
        cpu_data.task_start_time.store(start_time, Ordering::Relaxed);

        // Switch to the task
        let task_cx_ptr = &*task.mm.task_cx.lock() as *const _;
        let idle_cx_ptr = &mut *cpu_data.idle_context.lock() as *mut _;

        unsafe {
            crate::task::__switch(idle_cx_ptr, task_cx_ptr);
        }

        // Task has been switched away, update statistics
        let end_time = crate::timer::get_time_us();
        let runtime = end_time.saturating_sub(start_time);
        cpu_data.record_task_execution(runtime, 0); // Simplified for now

        // Clear current task
        cpu_data.set_current_task(None);
    }
}

/// Try to steal work from other CPUs
fn try_steal_task(cpu_id: usize) -> Option<Arc<crate::task::TaskControlBlock>> {
    // Implement work-stealing algorithm
    for victim_cpu in 0..MAX_CPU_NUM {
        if victim_cpu == cpu_id || !crate::smp::cpu_is_online(victim_cpu) {
            continue;
        }

        if let Some(victim_data) = cpu_data(victim_cpu) {
            // Only steal if victim has more than one task
            if victim_data.queue_length() > 1 {
                let stolen_tasks = victim_data.steal_tasks(1);
                if let Some(task) = stolen_tasks.into_iter().next() {
                    debug!("CPU {} stole task from CPU {}", cpu_id, victim_cpu);
                    return Some(task);
                }
            }
        }
    }
    None
}

/// Put secondary CPU in idle state
fn secondary_cpu_idle(cpu_id: usize) {
    if let Some(cpu_data) = cpu_data(cpu_id) {
        cpu_data.set_state(CpuState::Idle);

        let idle_start = crate::timer::get_time_us();

        // Wait for interrupt or work
        #[cfg(target_arch = "riscv64")]
        unsafe {
            riscv::asm::wfi();
        }

        let idle_end = crate::timer::get_time_us();
        let idle_time = idle_end.saturating_sub(idle_start);
        cpu_data.record_idle_time(idle_time);

        cpu_data.set_state(CpuState::Online);
    }
}

/// Stop a secondary CPU
fn secondary_cpu_stop(cpu_id: usize) -> ! {
    info!("Stopping CPU {}", cpu_id);

    if let Some(cpu_data) = cpu_data(cpu_id) {
        cpu_data.set_state(CpuState::Offline);
    }

    // Mark CPU as offline
    crate::smp::cpu_set_offline(cpu_id);

    // Disable interrupts and halt
    #[cfg(target_arch = "riscv64")]
    unsafe {
        riscv::interrupt::disable();
        loop {
            riscv::asm::wfi();
        }
    }

    #[cfg(not(target_arch = "riscv64"))]
    loop {
        core::hint::spin_loop();
    }
}

/// Halt a secondary CPU due to error
fn secondary_cpu_halt(cpu_id: usize) -> ! {
    error!("Halting CPU {} due to error", cpu_id);
    secondary_cpu_stop(cpu_id)
}

/// Set DTB address for secondary CPUs
pub fn set_dtb_addr(dtb_addr: usize) {
    DTB_ADDR.store(dtb_addr, Ordering::Release);
}

/// Start secondary CPUs using SBI Hart State Management
pub fn start_secondary_cpus() -> Result<usize, &'static str> {
    let topology = crate::smp::topology::get_topology()
        .ok_or("CPU topology not discovered")?;

    let mut started_count = 0;

    // Mark primary CPU as ready
    PRIMARY_CPU_READY.store(true, Ordering::Release);

    for cpu_info in &topology.cpus {
        // Skip the bootstrap processor (CPU 0)
        if cpu_info.cpu_id == 0 {
            continue;
        }

        info!("Starting CPU {} (hart {})", cpu_info.cpu_id, cpu_info.arch_id);

        // Use SBI to start the secondary CPU with proper entry point
        // 声明外部汇编函数
        unsafe extern "C" {
            fn _secondary_start();
        }

        let dtb_addr = DTB_ADDR.load(Ordering::Acquire);
        let result = sbi::hart_start(
            cpu_info.arch_id,
            _secondary_start as usize,
            dtb_addr, // Pass DTB address as opaque parameter
        );

        match result {
            Ok(_) => {
                started_count += 1;
                debug!("Successfully started CPU {}", cpu_info.cpu_id);
            }
            Err(e) => {
                warn!("Failed to start CPU {}: {:?}", cpu_info.cpu_id, e);
            }
        }
    }

    if started_count > 0 {
        info!("Waiting for {} secondary CPUs to initialize...", started_count);

        // Wait for all secondary CPUs to be ready (with timeout)
        let timeout_ms = SECONDARY_CPU_BOOT_TIMEOUT_MS;
        let start_time = crate::timer::get_time_us();

        while SECONDARY_CPU_READY_COUNT.load(Ordering::Acquire) < started_count {
            let elapsed = (crate::timer::get_time_us() - start_time) / 1000;
            if elapsed > timeout_ms {
                warn!("Timeout waiting for secondary CPUs, {} ready out of {}",
                      SECONDARY_CPU_READY_COUNT.load(Ordering::Acquire), started_count);
                break;
            }

            // Yield to allow other operations
            core::hint::spin_loop();
        }

        let ready_count = SECONDARY_CPU_READY_COUNT.load(Ordering::Acquire);
        info!("{} out of {} secondary CPUs are ready", ready_count, started_count);
    }

    Ok(started_count)
}

/// Check if all CPUs are online and ready
pub fn all_cpus_ready() -> bool {
    let topology = match crate::smp::topology::get_topology() {
        Some(t) => t,
        None => return false,
    };

    let expected_secondary_cpus = topology.cpu_count.saturating_sub(1);
    let ready_secondary_cpus = SECONDARY_CPU_READY_COUNT.load(Ordering::Acquire);

    ready_secondary_cpus >= expected_secondary_cpus
}

/// Wait for all CPUs to be online (used by primary CPU)
pub fn wait_for_all_cpus_online() {
    let topology = match crate::smp::topology::get_topology() {
        Some(t) => t,
        None => {
            warn!("No topology information available");
            return;
        }
    };

    let expected_cpus = topology.cpu_count;
    if expected_cpus <= 1 {
        debug!("Single CPU system, no waiting needed");
        return;
    }

    info!("Waiting for all {} CPUs to come online...", expected_cpus);

    let timeout_ms = SECONDARY_CPU_BOOT_TIMEOUT_MS;
    let start_time = crate::timer::get_time_us();

    loop {
        let online_count = crate::smp::cpu_count();
        if online_count >= expected_cpus {
            info!("All {} CPUs are now online", online_count);
            break;
        }

        let elapsed = (crate::timer::get_time_us() - start_time) / 1000;
        if elapsed > timeout_ms {
            warn!("Timeout waiting for CPUs, {} online out of {}",
                  online_count, expected_cpus);
            break;
        }

        // Brief delay before checking again
        for _ in 0..1000 {
            core::hint::spin_loop();
        }
    }
}

/// Shutdown all secondary CPUs
pub fn shutdown_secondary_cpus() -> Result<(), &'static str> {
    info!("Shutting down secondary CPUs");

    // Send stop IPIs to all secondary CPUs
    let result = crate::smp::ipi::send_stop_ipi_broadcast();

    match result {
        Ok(count) => {
            info!("Stop IPI sent to {} CPUs", count);

            // Wait a bit for CPUs to stop
            let wait_start = crate::timer::get_time_us();
            while (crate::timer::get_time_us() - wait_start) / 1000 < 1000 {
                core::hint::spin_loop();
            }

            Ok(())
        }
        Err(e) => {
            error!("Failed to send stop IPIs: {}", e);
            Err(e)
        }
    }
}

/// Get boot statistics
pub fn get_boot_stats() -> (usize, usize, usize) {
    let topology = crate::smp::topology::get_topology();
    let total_cpus = topology.map(|t| t.cpu_count).unwrap_or(1);
    let ready_cpus = SECONDARY_CPU_READY_COUNT.load(Ordering::Acquire) + 1; // +1 for primary
    let online_cpus = crate::smp::cpu_count();

    (total_cpus, ready_cpus, online_cpus)
}