#![no_std]
#![no_main]
#![feature(alloc_error_handler)]
#![allow(unused)]

extern crate alloc;

use alloc::sync::Arc;

mod arch;
mod config;
#[macro_use]
mod console;
#[macro_use]
mod log;

mod board;
mod drivers;
mod entry;
mod fs;
mod ipc;
mod lang_item;

mod id;
mod memory;
mod smp;
mod sync;
mod syscall;
mod task;
mod timer;
mod trap;
mod watchdog;

use crate::{
    log::LogLevel,
    smp::{current_cpu_id, init_cpu_id_register},
};

/// Global initialization that should only be done once by CPU0
fn global_init(dtb_addr: usize) -> Result<(), &'static str> {
    info!("Starting global system initialization");

    // Board and device tree initialization
    board::init(dtb_addr);

    // Global memory management
    memory::init();

    // SMP topology discovery
    smp::init();

    // Global filesystem
    fs::vfs::init_vfs();

    // Device drivers
    drivers::init_devices();

    // Global task management (scheduler, etc.)
    task::init();

    // Watchdog (global)
    watchdog::init();

    // Mark global initialization complete for secondary CPUs
    smp::boot::mark_global_init_complete();

    info!("Global system initialization complete");
    Ok(())
}

/// Per-CPU initialization that every CPU must do
fn per_cpu_init(cpu_id: usize, hart_id: usize) -> Result<(), &'static str> {
    debug!("Starting per-CPU initialization for CPU {}", cpu_id);

    // CPU-local logging (if needed)
    if cpu_id == 0 {
        log::init_auto();
        log::set_log_level(config::DEFAULT_LOG_LEVEL);
        log::disable_fs_logs();
        log::disable_memory_logs();
    }

    // Initialize per-CPU data structure for secondary CPUs
    if cpu_id > 0 {
        let cpu_data = Arc::new(smp::cpu::CpuData::new(cpu_id, smp::cpu::CpuType::Application));
        smp::set_cpu_data(cpu_id, cpu_data.clone());
        debug!("Created per-CPU data for CPU {}", cpu_id);
    }

    // Mark this CPU as online early
    smp::cpu_set_online(cpu_id);
    if let Some(cpu_data) = smp::cpu_data(cpu_id) {
        cpu_data.set_state(smp::cpu::CpuState::Online);
    }

    // Per-CPU memory management
    if cpu_id > 0 {
        // Secondary CPUs need memory initialization
        crate::smp::boot::secondary_cpu_memory_init(cpu_id)?;
    }

        // Per-CPU architecture-specific setup (including interrupts)
    if let Err(e) = crate::smp::boot::arch_specific_secondary_init(hart_id) {
        error!("Architecture-specific initialization failed for CPU {}: {}", cpu_id, e);
        return Err(e);
    }

    // Per-CPU trap handling
    trap::init();

    // Per-CPU timer
    if cpu_id == 0 {
        timer::init();
    } else {
        timer::init_secondary_cpu(cpu_id);
    }

    debug!("Per-CPU initialization complete for CPU {}", cpu_id);
    Ok(())
}

/// Unified CPU entry point for both primary and secondary CPUs
pub fn unified_cpu_main(hart_id: usize, dtb_addr: usize, is_primary: bool) -> ! {
    // Convert hart ID to logical CPU ID
    let cpu_id = if let Some(logical_id) = crate::smp::topology::arch_to_logical_cpu_id(hart_id) {
        logical_id
    } else {
        // Fallback for unknown hart IDs
        hart_id
    };

    // Initialize CPU ID register
    init_cpu_id_register(cpu_id);

    if is_primary {
        // CPU0: Do global initialization first, then per-CPU
        if let Err(e) = global_init(dtb_addr) {
            panic!("Global initialization failed: {}", e);
        }

        if let Err(e) = per_cpu_init(cpu_id, hart_id) {
            panic!("Per-CPU initialization failed for CPU {}: {}", cpu_id, e);
        }

        // Set DTB address for secondary CPUs
        smp::boot::set_dtb_addr(dtb_addr);

                        // Start secondary CPUs
        match smp::boot::start_secondary_cpus() {
            Ok(count) => {
                debug!("Started {} secondary CPUs", count);
            }
            Err(e) => {
                error!("Failed to start some secondary CPUs: {}", e);
            }
        }

        smp::boot::wait_for_all_cpus_online();
        print_system_info();

        // Start primary CPU task loop
        task::run_tasks();
    } else {
        // Secondary CPU: Only per-CPU initialization
        info!("Secondary CPU {} (hart {}) starting unified initialization", cpu_id, hart_id);

        // Wait for global initialization to complete
        while !smp::boot::global_init_complete() {
            core::hint::spin_loop();
        }

        if let Err(e) = per_cpu_init(cpu_id, hart_id) {
            error!("Per-CPU initialization failed for CPU {}: {}", cpu_id, e);
            smp::boot::secondary_cpu_halt(cpu_id);
        }

        info!("Secondary CPU {} initialization complete", cpu_id);

                // Signal that this CPU is ready
        smp::boot::mark_secondary_cpu_ready();

        // Start secondary CPU task loop
        task::run_tasks();
    }
}

#[unsafe(no_mangle)]
extern "C" fn kmain(hart_id: usize, dtb_addr: usize) -> ! {
    unified_cpu_main(hart_id, dtb_addr, true)
}

/// Print system information after initialization
fn print_system_info() {
    if config::DEFAULT_LOG_LEVEL != LogLevel::Debug {
        return;
    }

    let cpu_count = smp::cpu_count();
    let online_cpus = smp::online_cpu_ids();

    info!("=== System Information ===");
    info!("CPU Count: {}", cpu_count);
    info!("Online CPUs: {:?}", online_cpus);

    // Print topology information
    smp::topology::print_topology_info();

    // Print memory information
    let board_info = board::board_info();
    info!(
        "Memory: {:#x} - {:#x} ({}MB)",
        board_info.mem.start,
        board_info.mem.end,
        (board_info.mem.end - board_info.mem.start) >> 20
    );

    if let Some(topology) = smp::topology::get_topology() {
        info!("NUMA Nodes: {}", topology.numa_nodes.len());
        info!("Cache Levels: {}", topology.caches.len());
    }

    info!("========================");
}
