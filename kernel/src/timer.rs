use riscv::register;
use spin::Once;

use crate::{arch::sbi, config};

static mut TICKS: usize = 0;

static TICK_INTERVAL: Once<u64> = Once::new();

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
    let next_mtime = current_mtime + TICK_INTERVAL.wait();

    let _ = sbi::set_timer(next_mtime as usize);
}

pub fn init_timer_interrupt(time_base_freq: u64) {
    unsafe {
        // 使能中断
        register::sie::set_stimer();
    }

    TICK_INTERVAL.call_once(|| time_base_freq / config::TICKS_PER_SEC as u64);

    set_next_timer_interrupt();
}
