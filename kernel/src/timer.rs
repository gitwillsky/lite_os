use riscv::register;

use crate::{arch::sbi, board, config};

static mut TICKS: usize = 0;

static mut TICK_INTERVAL_VALUE: u64 = 0;

const MSEC_PER_SEC: u64 = 1000;

pub fn get_time_msec() -> u64 {
    let current_mtime = register::time::read64();
    let time_base_freq = board::get_board_info().time_base_freq;
    current_mtime / time_base_freq / MSEC_PER_SEC
}

#[inline(always)]
pub fn set_next_timer_interrupt() {
    let current_mtime = register::time::read64();
    let next_mtime = current_mtime + unsafe { TICK_INTERVAL_VALUE };

    let _ = sbi::set_timer(next_mtime as usize);
}

pub fn init() {
    let time_base_freq = board::get_board_info().time_base_freq;

    unsafe {
        TICK_INTERVAL_VALUE = time_base_freq / config::TICKS_PER_SEC as u64;
    }

    set_next_timer_interrupt();
    println!("timer initialized");
}
