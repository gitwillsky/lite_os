#![no_std]
#![no_main]
#![feature(alloc_error_handler)]

use alloc::boxed::Box;
use alloc::vec::Vec;
use riscv::asm::wfi;

extern crate alloc;

mod arch;
mod config;
#[macro_use]
mod console;
mod board;
mod entry;
mod lang_item;
mod memory;
mod process;
mod syscall;
mod timer;
mod trap;

#[unsafe(no_mangle)]
extern "C" fn kmain(_hart_id: usize, dtb_addr: usize) -> ! {
    board::init(dtb_addr);
    trap::init();
    timer::init();
    process::init();
    memory::init();

    println!("[HEAP TEST] Attempting to exhaust memory by repeated small allocations...");


    println!("[kernel] Interrupts enabled, Kernel is running...");

    loop {
        wfi();
    }
}
