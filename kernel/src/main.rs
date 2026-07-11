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
extern "C" fn kmain_boot(hart_id: usize, dtb_addr: usize) -> ! {
    init_local_arch(hart_id);

    log::init(config::DEFAULT_LOG_LEVEL);
    log::disable_module("kernel::task::loader");
    arch::dtb::init(dtb_addr);
    arch::hart::validate_boot_hart(arch::dtb::board_info(), hart_id);
    arch::sbi::verify_required_extensions();

    memory::init_allocator();
    arch::hart::init_topology(arch::dtb::board_info(), hart_id);
    debug!(
        "dynamic hart topology initialized: count={}, mask={:#x}, max_id={}",
        arch::hart::hart_count(),
        arch::hart::possible_hart_mask(),
        arch::hart::max_hart_id()
    );
    memory::init();
    timer::init_rtc();
    fs::vfs::init();
    drivers::init();
    task::init();

    // Release 发布页表、设备、文件系统和首个任务；secondary 在进入任何共享子系统前消费它。
    INIT_READY.store(true, Ordering::Release);
    for state in arch::hart::states() {
        if state.hart_id() == hart_id {
            continue;
        }
        arch::sbi::hart_start(state.hart_id(), arch::hart_start_entry(), dtb_addr).unwrap_or_else(
            |error| {
                panic!(
                    "SBI HSM failed to start hart {}: {}",
                    state.hart_id(),
                    error
                )
            },
        );
    }

    enter_scheduler()
}

#[unsafe(no_mangle)]
extern "C" fn kmain_secondary(hart_id: usize, dtb_addr: usize) -> ! {
    init_local_arch(hart_id);
    // Acquire 消费 boot hart 在 INIT_READY 之前完成的全部全局初始化写入。
    while !INIT_READY.load(Ordering::Acquire) {
        core::hint::spin_loop();
    }
    assert_eq!(
        dtb_addr,
        arch::dtb::board_info().dtb.start,
        "secondary received a different DTB opaque"
    );
    KERNEL_SPACE.wait().lock().active();

    enter_scheduler()
}

fn init_local_arch(hart_id: usize) {
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
}

fn enter_scheduler() -> ! {
    timer::enable_timer_interrupt();
    unsafe {
        register::sie::set_ssoft();
        register::sie::set_sext();
        register::sstatus::set_sie();
    }
    arch::hart::mark_online();
    if arch::hart::hart_id() == arch::hart::boot_hart_id() {
        // boot hart 等待所有 HSM target 完成本地初始化；缺失该屏障会把“hart_start 已接受”误当成 online。
        while arch::hart::online_hart_mask() != arch::hart::possible_hart_mask() {
            core::hint::spin_loop();
        }
        info!(
            "all DTB harts online: count={}, mask={:#x}",
            arch::hart::hart_count(),
            arch::hart::online_hart_mask()
        );
    }
    // 每个 hart 上线时同步一次共享 kernel 页表。除建立一致性外，这也保证 firmware
    // RFENCE 的同步完成路径在进入长期调度前已实际经过，而不是保留未连接的实现。
    memory::mm::MemorySet::flush_tlb_all_cpus()
        .expect("SBI RFENCE failed during per-hart activation");

    task::run_tasks();
}
