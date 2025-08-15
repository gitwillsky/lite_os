#![no_std]
#![no_main]
#![feature(alloc_error_handler)]
#![allow(unused)]

use crate::memory::KERNEL_SPACE;
use riscv::register;

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
mod signal;
mod syscall;
mod task;
mod timer;
mod trap;
mod watchdog;

#[unsafe(no_mangle)]
extern "C" fn kmain(hart_id: usize, dtb_addr: usize) -> ! {
    // 立即设置当前核心的TP寄存器，这样hart_id()函数就能正常工作
    crate::arch::hart::set_hart_id(hart_id);

    if hart_id == 0 {
        // 完整系统初始化
        log::init(config::DEFAULT_LOG_LEVEL);
        // log::disable_module("kernel::fs::fat32");
        log::disable_module("kernel::task::loader");
        // log::disable_module("kernel::drivers::device_manager");
        board::init(dtb_addr);
        trap::init();
        memory::init();
        timer::init_rtc();
        timer::enable_timer_interrupt();
        // 使能外部中断与全局中断
        unsafe {
            // 启用软件中断，用于处理IPI
            register::sie::set_ssoft();
            register::sie::set_sext();
            register::sstatus::set_sie();
        }
        watchdog::init();
        fs::vfs::init();
        drivers::init();
        signal::init();
        task::init();

        task::run_tasks();
    } else {
        board::init(dtb_addr);
        trap::init();
        KERNEL_SPACE.wait().lock().active();
        timer::enable_timer_interrupt();
        // 使能外部中断与全局中断（次核）
        unsafe {
            // 启用软件中断，用于处理IPI
            register::sie::set_ssoft();
            register::sie::set_sext();
            register::sstatus::set_sie();
        }

        task::run_tasks();
    }
}
