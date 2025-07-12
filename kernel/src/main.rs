#![no_std]
#![no_main]
#![feature(alloc_error_handler)]

use core::arch::global_asm;

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
mod lang_item;

mod loader;
mod memory;
mod sync;
mod syscall;
mod task;
mod timer;
mod trap;

global_asm!(include_str!("link_app.S"));

#[unsafe(no_mangle)]
extern "C" fn kmain(_hart_id: usize, dtb_addr: usize) -> ! {
    debug!("Kernel main entry, dtb_addr: {:#x}", dtb_addr);
    log::init(config::DEFAULT_LOG_LEVEL);

    board::init(dtb_addr);
    trap::init();
    memory::init();
    timer::init();
    fs::vfs::init_vfs();
    drivers::init_devices();
    task::init();
    task::run_tasks();
}
