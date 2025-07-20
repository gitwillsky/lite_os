use core::{
    panic::PanicInfo,
    sync::atomic::{AtomicBool, Ordering},
};

use riscv::register;

use crate::arch::sbi;

/// 简单的堆栈回溯实现
fn print_stack_trace() {
    // 获取当前寄存器状态
    let mut fp: usize;
    let mut ra: usize;
    let mut sp: usize;

    unsafe {
        // 获取帧指针、返回地址和栈指针
        core::arch::asm!("mv {}, s0", out(reg) fp);
        core::arch::asm!("mv {}, ra", out(reg) ra);
        core::arch::asm!("mv {}, sp", out(reg) sp);
    }

    error!("Register State:");
    error!("  RA (Return Address): {:#x}", ra);
    error!("  FP (Frame Pointer):  {:#x}", fp);
    error!("  SP (Stack Pointer):  {:#x}", sp);
}

#[panic_handler]
fn panic_handler(info: &PanicInfo) -> ! {
    // Disable interrupts
    unsafe {
        register::sstatus::clear_sie();
    }

    if let Some(location) = info.location() {
        error!(
            "[Kernel] Panic at {}:{}:{} {}",
            location.file(),
            location.line(),
            location.column(),
            info.message()
        );
    } else {
        error!("[Kernel] Panic: {}", info.message());
    }

    // 打印堆栈跟踪
    print_stack_trace();

    _ = sbi::shutdown();

    #[allow(unreachable_code)]
    loop {
        riscv::asm::wfi();
    }
}
