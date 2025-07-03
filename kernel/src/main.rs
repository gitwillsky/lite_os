#![no_std]
#![no_main]
#![feature(alloc_error_handler)]

use core::arch::global_asm;
use riscv::asm::wfi;

extern crate alloc;

mod arch;
mod config;
#[macro_use]
mod console;
mod board;
mod entry;
mod lang_item;
mod loader;
mod memory;
mod syscall;
mod task;
mod timer;
mod trap;

global_asm!(include_str!("link_app.S"));

#[unsafe(no_mangle)]
extern "C" fn kmain(_hart_id: usize, dtb_addr: usize) -> ! {
    println!("[kmain] entry");
    board::init(dtb_addr);
    println!("[kmain] after board::init");
    trap::init();
    println!("[kmain] after trap::init");
    memory::init();
    println!("[kmain] after memory::init");
    timer::init();
    println!("[kmain] after timer::init");
    task::init();
    println!("[kmain] after task::init");
    println!("[kernel] Interrupts enabled, Kernel is running...");

    task::run_first_task();

    loop {
        wfi();
    }
}
