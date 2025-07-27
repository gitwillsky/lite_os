/// SMP boot sequence implementation
///
/// This module handles the multi-core boot process, including secondary
/// CPU initialization and synchronization between cores.

use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicUsize, AtomicBool, Ordering};
use crate::{
    smp::{
        cpu::{CpuData, CpuState, CpuType},
        current_cpu_id, set_cpu_data, cpu_set_online, cpu_data,
        MAX_CPU_NUM, init_cpu_id_register,
        ipi::{self, create_ipi_barrier, wait_at_ipi_barrier}
    },
    arch::sbi,
    memory::{address::PhysicalAddress, KERNEL_SPACE, TlbManager},
    sync::spinlock::SpinLock,
    timer::get_time_msec,
};

static DTB_ADDR: AtomicUsize = AtomicUsize::new(0);

/// Enhanced boot synchronization using IPI barriers
static BOOT_PHASE_BARRIER: SpinLock<Option<u64>> = SpinLock::new(None);
static MEMORY_INIT_BARRIER: SpinLock<Option<u64>> = SpinLock::new(None);
static SYSTEM_READY_BARRIER: SpinLock<Option<u64>> = SpinLock::new(None);
static PRIMARY_CPU_READY: AtomicBool = AtomicBool::new(false);

/// Legacy counter for compatibility (will be removed in future)
static SECONDARY_CPU_READY_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Boot phase tracking
static CURRENT_BOOT_PHASE: SpinLock<BootPhase> = SpinLock::new(BootPhase::Initialization);

/// Boot phases for coordinated initialization
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BootPhase {
    Initialization,
    MemorySetup,
    SystemReady,
}

/// Maximum time to wait for secondary CPUs to boot (in milliseconds)
const SECONDARY_CPU_BOOT_TIMEOUT_MS: u64 = 10000;

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

    info!("Secondary CPU {} (hart {}) starting enhanced initialization", cpu_id, hart_id);

    // Wait for primary CPU to complete global initialization
    while !PRIMARY_CPU_READY.load(Ordering::Acquire) {
        core::hint::spin_loop();
    }

    // Initialize this CPU using traditional method first
    secondary_cpu_init(cpu_id, hart_id);

    // Mark this CPU as ready for legacy compatibility
    SECONDARY_CPU_READY_COUNT.fetch_add(1, Ordering::AcqRel);

    info!("Secondary CPU {} enhanced initialization complete", cpu_id);

    // Enter the unified scheduler loop
    crate::task::run_tasks()
}

/// Phased secondary CPU initialization using IPI barriers
fn secondary_cpu_init_phased(cpu_id: usize, hart_id: usize) -> Result<(), &'static str> {
    // Phase 1: Basic initialization
    if let Err(e) = wait_for_boot_phase(BootPhase::Initialization) {
        error!("CPU{} failed to synchronize in initialization phase: {}", cpu_id, e);
        return Err("Initialization phase sync failed");
    }

    // Set CPU state to starting
    if let Some(cpu_data) = cpu_data(cpu_id) {
        cpu_data.set_state(CpuState::Starting);
    } else {
        return Err("No CPU data available");
    }

    // Architecture-specific initialization
    if let Err(e) = arch_specific_secondary_init(hart_id) {
        error!("Architecture-specific initialization failed for CPU {}: {}", cpu_id, e);
        return Err(e);
    }

    // Phase 2: Memory management initialization
    if let Err(e) = wait_for_boot_phase(BootPhase::MemorySetup) {
        error!("CPU{} failed to synchronize in memory setup phase: {}", cpu_id, e);
        return Err("Memory setup phase sync failed");
    }

    if let Err(e) = secondary_cpu_memory_init(cpu_id) {
        error!("Memory initialization failed for CPU {}: {}", cpu_id, e);
        return Err(e);
    }

    if let Err(e) = secondary_cpu_timer_init(cpu_id) {
        error!("Timer initialization failed for CPU {}: {}", cpu_id, e);
        return Err(e);
    }

    // Phase 3: System ready
    if let Err(e) = wait_for_boot_phase(BootPhase::SystemReady) {
        error!("CPU{} failed to synchronize in system ready phase: {}", cpu_id, e);
        return Err("System ready phase sync failed");
    }

    // Mark CPU as online
    cpu_set_online(cpu_id);
    if let Some(cpu_data) = cpu_data(cpu_id) {
        cpu_data.set_state(CpuState::Online);
        debug!("CPU{} marked as online with enhanced synchronization", cpu_id);
    }

    Ok(())
}

/// Wait for a specific boot phase using IPI barriers
fn wait_for_boot_phase(phase: BootPhase) -> Result<(), &'static str> {
    let barrier_id = match phase {
        BootPhase::Initialization => {
            BOOT_PHASE_BARRIER.lock().ok_or("No initialization barrier")?
        }
        BootPhase::MemorySetup => {
            MEMORY_INIT_BARRIER.lock().ok_or("No memory setup barrier")?
        }
        BootPhase::SystemReady => {
            SYSTEM_READY_BARRIER.lock().ok_or("No system ready barrier")?
        }
    };

    debug!("CPU{} waiting for phase {:?}", current_cpu_id(), phase);
    wait_at_ipi_barrier(barrier_id)
}

/// Initialize a secondary CPU
fn secondary_cpu_init(cpu_id: usize, hart_id: usize) {
    // Set CPU state to starting
    if let Some(cpu_data) = cpu_data(cpu_id) {
        cpu_data.set_state(CpuState::Starting);
    } else {
        error!("No CPU data available for CPU {} during initialization", cpu_id);
        secondary_cpu_halt(cpu_id);
    }

    // Initialize architecture-specific features
    if let Err(e) = arch_specific_secondary_init(hart_id) {
        error!("Architecture-specific initialization failed for CPU {}: {}", cpu_id, e);
        secondary_cpu_halt(cpu_id);
    }

    // Initialize per-CPU memory management
    if let Err(e) = secondary_cpu_memory_init(cpu_id) {
        error!("Memory initialization failed for CPU {}: {}", cpu_id, e);
        secondary_cpu_halt(cpu_id);
    }

    // Initialize per-CPU timer
    if let Err(e) = secondary_cpu_timer_init(cpu_id) {
        error!("Timer initialization failed for CPU {}: {}", cpu_id, e);
        secondary_cpu_halt(cpu_id);
    }

    // Mark CPU as online
    cpu_set_online(cpu_id);

    if let Some(cpu_data) = cpu_data(cpu_id) {
        cpu_data.set_state(CpuState::Online);
    } else {
        error!("No CPU data available for CPU {} after initialization", cpu_id);
        secondary_cpu_halt(cpu_id);
    }
}

/// Architecture-specific secondary CPU initialization
fn arch_specific_secondary_init(hart_id: usize) -> Result<(), &'static str> {
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
        Ok(())
    }
    
    #[cfg(not(target_arch = "riscv64"))]
    {
        warn!("Architecture-specific init not implemented for this architecture");
        Ok(())
    }
}

/// Initialize memory management for secondary CPU
fn secondary_cpu_memory_init(cpu_id: usize) -> Result<(), &'static str> {
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
    if let Some(_cpu_data) = cpu_data(cpu_id) {
        // Per-CPU allocator will be initialized on first use
        debug!("Memory management initialized for CPU {}", cpu_id);
        Ok(())
    } else {
        Err("CPU data not available for memory initialization")
    }
}

/// Initialize timer for secondary CPU
fn secondary_cpu_timer_init(cpu_id: usize) -> Result<(), &'static str> {
    // Timer initialization is handled by the timer module
    crate::timer::init_secondary_cpu(cpu_id);
    debug!("Timer initialized for CPU {}", cpu_id);
    Ok(())
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

/// Enhanced start secondary CPUs with fallback to traditional synchronization
pub fn start_secondary_cpus() -> Result<usize, &'static str> {
    let topology = crate::smp::topology::get_topology()
        .ok_or("CPU topology not discovered")?;

    let mut started_count = 0;

    info!("Starting enhanced multi-phase boot sequence for {} CPUs", topology.cpus.len());

    // First, start all secondary CPUs using traditional method
    for cpu_info in &topology.cpus {
        // Skip the bootstrap processor (CPU 0)
        if cpu_info.cpu_id == 0 {
            continue;
        }

        info!("Starting CPU {} (hart {})", cpu_info.cpu_id, cpu_info.arch_id);

        unsafe extern "C" {
            fn _secondary_start();
        }

        let dtb_addr = DTB_ADDR.load(Ordering::Acquire);
        let result = sbi::hart_start(
            cpu_info.arch_id,
            _secondary_start as usize,
            dtb_addr,
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
        info!("Waiting for {} secondary CPUs using traditional synchronization", started_count);

        // Mark primary CPU as ready to start coordination
        PRIMARY_CPU_READY.store(true, Ordering::Release);

        // Wait for secondary CPUs to start with timeout
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

        // Now try to initialize enhanced IPI barriers for online CPUs
        if ready_count > 0 {
            initialize_enhanced_synchronization(ready_count + 1); // +1 for primary CPU
        }

        // Initialize IPI subsystem now that CPUs are online
        ipi::init();
        
        // Perform initial system health check
        perform_initial_health_check();
    }

    Ok(started_count)
}

/// Initialize enhanced synchronization after CPUs are online
fn initialize_enhanced_synchronization(online_cpu_count: usize) {
    info!("Initializing enhanced IPI synchronization for {} online CPUs", online_cpu_count);

    // Build list of actually online CPUs
    let mut online_cpus = Vec::new();
    for cpu_id in 0..crate::smp::cpu_count() {
        if let Some(cpu_data) = cpu_data(cpu_id) {
            if cpu_data.state() == CpuState::Online || cpu_id == 0 {
                online_cpus.push(cpu_id);
            }
        }
    }

    if online_cpus.len() < 2 {
        debug!("Not enough CPUs online for enhanced synchronization");
        return;
    }

    // Try to create IPI barriers for online CPUs only
    match create_ipi_barrier(&online_cpus, 5000) { // 5 second timeout
        Ok(test_barrier) => {
            info!("Successfully created IPI barrier for {} CPUs", online_cpus.len());
            
            // Test the barrier
            match wait_at_ipi_barrier(test_barrier) {
                Ok(_) => {
                    info!("IPI barrier test successful");
                    // Store a new barrier for future use
                    match create_ipi_barrier(&online_cpus, SECONDARY_CPU_BOOT_TIMEOUT_MS) {
                        Ok(barrier) => {
                            *SYSTEM_READY_BARRIER.lock() = Some(barrier);
                            info!("Enhanced synchronization initialized successfully");
                        }
                        Err(e) => {
                            warn!("Failed to create persistent barrier: {}", e);
                        }
                    }
                }
                Err(e) => {
                    warn!("IPI barrier test failed: {}", e);
                }
            }
        }
        Err(e) => {
            warn!("Failed to create IPI barrier for enhanced synchronization: {}", e);
            info!("Falling back to traditional synchronization methods");
        }
    }
}

/// Perform initial system health check after all CPUs are online
fn perform_initial_health_check() {
    let mut online_cpus = 0;
    let mut failed_cpus = Vec::new();

    for cpu_id in 0..crate::smp::cpu_count() {
        if let Some(cpu_data) = cpu_data(cpu_id) {
            match cpu_data.state() {
                CpuState::Online => {
                    online_cpus += 1;
                    debug!("CPU{} is online and healthy", cpu_id);
                }
                state => {
                    warn!("CPU{} is in unexpected state: {:?}", cpu_id, state);
                    failed_cpus.push(cpu_id);
                }
            }
        } else {
            error!("No CPU data available for CPU{}", cpu_id);
            failed_cpus.push(cpu_id);
        }
    }

    info!("System health check: {} CPUs online, {} failed: {:?}", 
          online_cpus, failed_cpus.len(), failed_cpus);

    // Test IPI functionality between CPUs
    test_ipi_connectivity();
}

/// Test IPI connectivity between all CPUs
fn test_ipi_connectivity() {
    info!("Testing IPI connectivity between CPUs");
    let current_cpu = current_cpu_id();
    let mut successful_tests = 0;
    let mut failed_tests = 0;

    for cpu_id in 0..crate::smp::cpu_count() {
        if cpu_id == current_cpu {
            continue;
        }

        match ipi::send_function_call_ipi_sync(cpu_id, || {
            debug!("IPI connectivity test received on CPU{}", current_cpu_id());
            ipi::IpiResponse::Success
        }, 1000) {
            Ok(ipi::IpiResponse::Success) => {
                successful_tests += 1;
                debug!("IPI test successful: CPU{} -> CPU{}", current_cpu, cpu_id);
            }
            Ok(response) => {
                failed_tests += 1;
                warn!("IPI test unexpected response: CPU{} -> CPU{}: {:?}", 
                      current_cpu, cpu_id, response);
            }
            Err(e) => {
                failed_tests += 1;
                error!("IPI test failed: CPU{} -> CPU{}: {}", current_cpu, cpu_id, e);
            }
        }
    }

    info!("IPI connectivity test complete: {} successful, {} failed", 
          successful_tests, failed_tests);
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

/// Enhanced shutdown all secondary CPUs with synchronous confirmation
pub fn shutdown_secondary_cpus() -> Result<(), &'static str> {
    info!("Shutting down secondary CPUs with enhanced synchronization");

    let current_cpu = current_cpu_id();
    let mut shutdown_confirmations = 0;
    let mut failed_shutdowns = 0;

    // Send synchronous stop IPIs to all secondary CPUs
    for cpu_id in 0..crate::smp::cpu_count() {
        if cpu_id == current_cpu {
            continue; // Skip current CPU
        }

        if let Some(cpu_data) = cpu_data(cpu_id) {
            if cpu_data.state() != CpuState::Online {
                debug!("CPU{} is not online, skipping shutdown", cpu_id);
                continue;
            }

            // Send synchronous stop IPI with confirmation
            match ipi::send_stop_ipi_sync(cpu_id, 2000) { // 2 second timeout
                Ok(ipi::IpiResponse::Success) => {
                    shutdown_confirmations += 1;
                    info!("CPU{} confirmed shutdown", cpu_id);
                }
                Ok(response) => {
                    failed_shutdowns += 1;
                    warn!("CPU{} shutdown unexpected response: {:?}", cpu_id, response);
                }
                Err(e) => {
                    failed_shutdowns += 1;
                    error!("Failed to shutdown CPU{}: {}", cpu_id, e);
                }
            }
        }
    }

    // Clean up IPI resources
    ipi::cleanup_expired_ipi_resources();

    info!("Enhanced shutdown complete: {} confirmed, {} failed", 
          shutdown_confirmations, failed_shutdowns);

    if failed_shutdowns > 0 {
        warn!("Some CPUs failed to shutdown cleanly");
    }

    Ok(())
}

/// Get boot statistics
pub fn get_boot_stats() -> (usize, usize, usize) {
    let topology = crate::smp::topology::get_topology();
    let total_cpus = topology.map(|t| t.cpu_count).unwrap_or(1);
    let ready_cpus = SECONDARY_CPU_READY_COUNT.load(Ordering::Acquire) + 1; // +1 for primary
    let online_cpus = crate::smp::cpu_count();

    (total_cpus, ready_cpus, online_cpus)
}