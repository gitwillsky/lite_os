#![no_std]
#![no_main]
#![feature(alloc_error_handler)]
#![allow(unused)]

extern crate alloc;

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

#[unsafe(no_mangle)]
extern "C" fn kmain(hart_id: usize, dtb_addr: usize) -> ! {
    log::init_auto();
    log::set_log_level(config::DEFAULT_LOG_LEVEL);

    init_cpu_id_register(hart_id);
    primary_cpu_init(hart_id, dtb_addr);
}

fn primary_cpu_init(hart_id: usize, dtb_addr: usize) -> ! {
    board::init(dtb_addr);
    memory::init();
    smp::init();
    trap::init();
    timer::init();
    watchdog::init();
    smp::ipi::init();
    fs::vfs::init_vfs();
    drivers::init_devices();
    task::init();

    // // 为secondary CPUs设置DTB地址
    // smp::boot::set_dtb_addr(dtb_addr);

    // match smp::boot::start_secondary_cpus() {
    //     Ok(count) => {
    //         debug!("Started {} secondary CPUs", count);
    //     }
    //     Err(e) => {
    //         error!("Failed to start some secondary CPUs: {}", e);
    //     }
    // }

    // smp::boot::wait_for_all_cpus_online();

    // Print final system information
    if config::DEFAULT_LOG_LEVEL == LogLevel::Debug {
        print_system_info();
    }

    task::run_tasks();
}

/// Print system information after initialization
fn print_system_info() {
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
