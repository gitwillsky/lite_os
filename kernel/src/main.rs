#![no_std]
#![no_main]
#![feature(alloc_error_handler)]

use core::sync::atomic::{AtomicBool, Ordering};
use crate::memory::KERNEL_SPACE;
use riscv::register;

extern crate alloc;
#[macro_use]
extern crate bitflags;

#[macro_use]
mod arch;
mod config;
#[macro_use]
mod log;

mod drivers;
mod fs;
mod ipc;
mod lang_item;

mod id;
mod memory;
mod signal;
mod syscall;
mod task;
mod timer;
mod trap;
mod watchdog;

/// 标记全局内核设施已完成初始化。
///
/// 次级 hart 不能仅等待内核页表，因为页表会在文件系统、驱动和首个用户任务
/// 就绪前发布；缺少此屏障会让次级 hart 提前进入调度器并访问未初始化的全局状态。
static INIT_READY: AtomicBool = AtomicBool::new(false);

#[unsafe(no_mangle)]
extern "C" fn kmain(hart_id: usize, dtb_addr: usize) -> ! {
    // 每个 hart 都必须启用浮点状态，否则用户态或内核态浮点指令会触发非法指令异常。
    unsafe {
        register::sstatus::set_fs(register::sstatus::FS::Dirty);
    }

    if hart_id == 0 {
        log::init(config::DEFAULT_LOG_LEVEL);
        log::disable_module("kernel::task::loader");
        arch::dtb::init(dtb_addr);
        trap::init();
        memory::init();
        timer::init_rtc();
        timer::enable_timer_interrupt();
        unsafe {
            register::sie::set_ssoft();
            register::sie::set_sext();
            register::sstatus::set_sie();
        }
        watchdog::init();
        fs::vfs::init();
        drivers::init();
        signal::init();
        task::init();
        INIT_READY.store(true, Ordering::Release);
    } else {
        arch::dtb::init(dtb_addr);
        trap::init();
        KERNEL_SPACE.wait().lock().active();
        while !INIT_READY.load(Ordering::Acquire) {
            core::hint::spin_loop();
        }
        timer::enable_timer_interrupt();
        unsafe {
            register::sie::set_ssoft();
            register::sie::set_sext();
            register::sstatus::set_sie();
        }
    }

    task::run_tasks();
}
