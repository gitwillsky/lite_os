#![no_std]
#![no_main]
#![feature(alloc_error_handler)]
#![allow(unused)]

use crate::memory::KERNEL_SPACE;

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
mod sync;
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
        board::init(dtb_addr);
        trap::init();
        memory::init();
        timer::init_rtc();
        timer::enable_timer_interrupt();
        watchdog::init();
        fs::vfs::init_vfs();
        drivers::init_devices();
        task::init();

        // 激活boot核心
        task::multicore::CORE_MANAGER.activate_core(hart_id);

        task::run_tasks();
    } else {
        trap::init();
        KERNEL_SPACE.wait().lock().active();
        timer::enable_timer_interrupt();

        // 激活从核心
        task::multicore::CORE_MANAGER.activate_core(hart_id);

        task::run_tasks();
    }
}
