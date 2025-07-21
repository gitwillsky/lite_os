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
mod id;

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
    print!("你好");
    task::run_tasks();
}
