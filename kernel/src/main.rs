#![no_std]
#![no_main]
#![feature(alloc_error_handler)]

use crate::memory::KERNEL_SPACE;
use core::sync::atomic::{AtomicBool, Ordering};
use riscv::register;

extern crate alloc;

#[macro_use]
mod arch;
mod config;
#[macro_use]
mod log;

mod drivers;
mod fs;
mod lang_item;

mod id;
mod memory;
mod sync;
mod syscall;
mod task;
mod timer;
mod trap;

/// 标记全局内核设施已完成初始化。
///
/// 次级 hart 不能仅等待内核页表，因为页表会在文件系统、驱动和首个用户任务
/// 就绪前发布；缺少此屏障会让次级 hart 提前进入调度器并访问未初始化的全局状态。
static INIT_READY: AtomicBool = AtomicBool::new(false);

#[unsafe(no_mangle)]
extern "C" fn kmain(hart_id: usize, dtb_addr: usize, is_boot_hart: usize) -> ! {
    // 每个 hart 都必须启用浮点状态，否则用户态或内核态浮点指令会触发非法指令异常。
    unsafe {
        register::sstatus::set_fs(register::sstatus::FS::Dirty);
    }
    assert_eq!(
        hart_id,
        arch::hart::raw_hart_id(),
        "SBI hart ID and kernel tp disagree"
    );

    trap::init();

    if is_boot_hart != 0 {
        log::init(config::DEFAULT_LOG_LEVEL);
        log::disable_module("kernel::task::loader");
        arch::dtb::init(dtb_addr);
        arch::hart::init_topology(arch::dtb::board_info(), hart_id);
        arch::sbi::verify_required_extensions();
        memory::init();
        timer::init_rtc();
        fs::vfs::init();
        drivers::init();
        task::init();
        // Release 发布页表、设备、文件系统和首个任务；缺失时 secondary
        // 可能在这些全局对象仍处于构造中时进入调度循环。
        INIT_READY.store(true, Ordering::Release);
    } else {
        // Acquire 消费 boot hart 在 INIT_READY 之前完成的全部全局初始化写入。
        while !INIT_READY.load(Ordering::Acquire) {
            core::hint::spin_loop();
        }
        KERNEL_SPACE.wait().lock().active();
    }

    timer::enable_timer_interrupt();
    unsafe {
        register::sie::set_ssoft();
        register::sie::set_sext();
        register::sstatus::set_sie();
    }
    arch::hart::mark_online();
    // 每个 hart 上线时同步一次共享 kernel 页表。除建立一致性外，这也保证 firmware
    // RFENCE 的同步完成路径在进入长期调度前已实际经过，而不是保留未连接的实现。
    memory::mm::MemorySet::flush_tlb_all_cpus()
        .expect("SBI RFENCE failed during per-hart activation");

    task::run_tasks();
}
