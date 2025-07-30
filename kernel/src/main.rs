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

mod memory;
mod sync;
mod syscall;
mod task;
mod timer;
mod trap;
mod watchdog;
mod id;

#[unsafe(no_mangle)]
extern "C" fn kmain(hart_id: usize, dtb_addr: usize) -> ! {
    if hart_id == 0 {
        primary_core_boot(hart_id, dtb_addr);
    } else {
        secondary_core_boot(hart_id, dtb_addr);
    }
}

/// 主核心启动流程
fn primary_core_boot(hart_id: usize, dtb_addr: usize) -> ! {
    debug!("Primary core {} boot, dtb_addr: {:#x}", hart_id, dtb_addr);
    
    // 完整系统初始化
    log::init(config::DEFAULT_LOG_LEVEL);
    board::init(dtb_addr);
    trap::init();
    memory::init();
    timer::init();
    watchdog::init();
    fs::vfs::init_vfs();
    drivers::init_devices();
    task::init();
    
    // 激活主核心
    task::multicore::CORE_MANAGER.activate_core(hart_id);
    
    // 启动从核心
    start_secondary_cores();
    
    info!("Primary core {} initialized, entering scheduler", hart_id);
    task::run_tasks();
}

/// 从核心启动流程
fn secondary_core_boot(hart_id: usize, dtb_addr: usize) -> ! {
    debug!("Secondary core {} boot, dtb_addr: {:#x}", hart_id, dtb_addr);
    
    // 等待主核心完成初始化
    wait_primary_init();
    
    // 初始化核心本地数据
    trap::init_local();
    
    // 激活从核心
    task::multicore::CORE_MANAGER.activate_core(hart_id);
    
    info!("Secondary core {} initialized, entering scheduler", hart_id);
    task::run_tasks();
}

/// 启动从核心
fn start_secondary_cores() {
    use crate::arch::{sbi, hart::MAX_CORES};
    
    // 检测可用的核心数量并启动
    for hart in 1..MAX_CORES {
        debug!("Starting secondary core {}", hart);
        match sbi::hart_start(hart, secondary_core_entry as usize, 0) {
            Ok(_) => {
                debug!("Secondary core {} start request sent", hart);
            }
            Err(e) => {
                debug!("Failed to start core {}: {:?}", hart, e);
                break; // 如果某个核心启动失败，停止尝试后续核心
            }
        }
    }
    
    // 等待一段时间让从核心启动
    for _ in 0..1000000 {
        core::hint::spin_loop();
    }
    
    info!("Active cores: {}", task::multicore::CORE_MANAGER.active_core_count());
}

/// 从核心入口点
#[unsafe(no_mangle)]
extern "C" fn secondary_core_entry() -> ! {
    let hart_id = crate::arch::hart::hart_id();
    secondary_core_boot(hart_id, 0);
}

/// 等待主核心完成初始化
fn wait_primary_init() {
    // 简单的自旋等待，等待主核心完成关键初始化
    // 通过检查某些全局状态来判断主核心是否就绪
    while task::multicore::CORE_MANAGER.active_core_count() == 0 {
        core::hint::spin_loop();
    }
}
