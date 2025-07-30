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

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

static SYSTEM_INITIALIZED: AtomicBool = AtomicBool::new(false);
static BOOT_HART: AtomicUsize = AtomicUsize::new(usize::MAX);

#[unsafe(no_mangle)]
extern "C" fn kmain(hart_id: usize, dtb_addr: usize) -> ! {
    // 立即设置当前核心的TP寄存器，这样hart_id()函数就能正常工作
    crate::arch::hart::set_hart_id(hart_id);
    
    // 使用原子操作选择第一个到达的核心作为boot核心
    match BOOT_HART.compare_exchange(usize::MAX, hart_id, Ordering::AcqRel, Ordering::Acquire) {
        Ok(_) => {
            // 这是第一个到达的核心，负责系统初始化
            boot_core_init(hart_id, dtb_addr);
        }
        Err(_) => {
            // 其他核心等待系统初始化完成
            secondary_core_init(hart_id, dtb_addr);
        }
    }
}

/// Boot核心初始化流程
fn boot_core_init(hart_id: usize, dtb_addr: usize) -> ! {
    // 完整系统初始化
    log::init(config::DEFAULT_LOG_LEVEL);

    debug!("Boot core {} initializing system, dtb_addr: {:#x}", hart_id, dtb_addr);
    board::init(dtb_addr);
    trap::init();
    memory::init();
    timer::init();
    watchdog::init();
    fs::vfs::init_vfs();
    drivers::init_devices();
    task::init();

    // 激活boot核心
    task::multicore::CORE_MANAGER.activate_core(hart_id);

    // 标记系统初始化完成
    SYSTEM_INITIALIZED.store(true, Ordering::Release);

    info!("Boot core {} system initialized, entering scheduler", hart_id);
    task::run_tasks();
}

/// 从核心初始化流程
fn secondary_core_init(hart_id: usize, dtb_addr: usize) -> ! {
    debug!("Secondary core {} waiting for system init, dtb_addr: {:#x}", hart_id, dtb_addr);

    // 等待系统初始化完成
    while !SYSTEM_INITIALIZED.load(Ordering::Acquire) {
        core::hint::spin_loop();
    }

    // 初始化核心本地数据
    trap::init_local();

    // 激活从核心
    task::multicore::CORE_MANAGER.activate_core(hart_id);

    info!("Secondary core {} initialized, entering scheduler", hart_id);
    task::run_tasks();
}

