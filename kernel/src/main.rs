#![no_std]
#![no_main]
use core::{arch::asm, panic::PanicInfo};

use arch::sbi;
use riscv::register;

#[macro_use]
mod console;
mod arch;
mod config;
mod entry;
mod memory;
mod process;
mod timer;
mod trap;

#[unsafe(no_mangle)]
extern "C" fn kmain() -> ! {
    trap::init();
    timer::init_timer_interrupt();
    memory::init();
    process::init();

    unsafe {
        register::sstatus::set_sie();
    }

    println!("[kernel] Interrupts enabled, Kernel is running...");

    loop {
        unsafe {
            asm!("wfi");
        }
    }
}

#[panic_handler]
fn panic_handler(info: &PanicInfo) -> ! {
    print!("Kernel panic: ");
    if let Some(location) = info.location() {
        print!("{}", location.file());
        print!(":");
        print!("{}", location.line());
        print!("\n");
    }
    if let Some(message) = info.message().as_str() {
        print!("{}", message);
    }
    sbi::shutdown();
}
