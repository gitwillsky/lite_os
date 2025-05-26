#![no_std]
#![no_main]

use board::BoardInfo;
use riscv::asm::wfi;

mod arch;
mod config;
#[macro_use]
mod console;
mod board;
mod entry;
mod lang_item;
mod memory;
mod process;
mod timer;
mod trap;

#[unsafe(no_mangle)]
extern "C" fn kmain(_hart_id: usize, dtb_addr: usize) -> ! {
    let board_info = BoardInfo::parse(dtb_addr);

    trap::init();
    timer::init_timer_interrupt(board_info.time_base_freq);
    memory::init();
    process::init();

    println!("[kernel] Interrupts enabled, Kernel is running...");

    loop {
        wfi();
    }
}
