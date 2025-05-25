use riscv::register;

use crate::{arch::sbi, config};

static mut TICKS: usize = 0;

pub fn handle_supervisor_timer_interrupt() {
    set_next_timer_interrupt();
    unsafe {
        TICKS += 1;
        if TICKS % config::TICKS_PER_SEC == 0 {
            println!("[kernel] {} seconds passed", TICKS / config::TICKS_PER_SEC);
        }
    }
}

fn set_next_timer_interrupt() {
    let current_mtime = register::time::read64();
    let next_mtime = current_mtime + config::TICK_INTERVAL as u64;

    let _ = sbi::set_timer(next_mtime as usize);
}

pub fn init_timer_interrupt() {
    unsafe {
        // 使能中断
        register::sie::set_stimer();
    }

    set_next_timer_interrupt();
}
