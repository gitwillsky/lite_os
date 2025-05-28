#![no_std]
#![no_main]
#![feature(alloc_error_handler)]

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
    memory::init();
    process::init();

    println!("[kernel] Interrupts enabled, Kernel is running...");

    loop {
        wfi();
    }
}
