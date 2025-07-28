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
    smp::current_cpu_id,
};

#[unsafe(no_mangle)]
extern "C" fn kmain(hart_id: usize, dtb_addr: usize) -> ! {
    log::init_auto();
    log::set_log_level(config::DEFAULT_LOG_LEVEL);
    log::disable_fs_logs();
    // log::disable_memory_logs();

    // Board and device tree initialization
    board::init(dtb_addr);

    // Global memory management
    memory::init();

    // Global timer
    timer::init();

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

    // Mark CPU0 (BSP) as online
    smp::cpu_set_online(0);
    if let Some(cpu0_data) = smp::cpu_data(0) {
        cpu0_data.set_state(smp::cpu::CpuState::Online);
        debug!("CPU0 (BSP) marked as online");
    }

    // Mark global initialization complete for secondary CPUs
    smp::boot::mark_global_init_complete();

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
