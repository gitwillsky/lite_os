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

    // 基于 RISC-V 常见帧布局尝试回溯：
    // prologue: addi sp,-framesz; sd ra,framesz-8(sp); sd s0,framesz-16(sp); addi s0,sp,framesz
    // 因此 [fp-8] 是保存的 ra，[fp-16] 是上一帧的 fp。
    let mut cur_fp = fp;
    for depth in 0..32 {
        if cur_fp < 0xFFFF_F000000000 { // 粗略过滤非法/非高半区地址，避免访问错误
            break;
        }
        // 安全访问保护：检查对齐
        if (cur_fp & 0x7) != 0 {
            break;
        }
        unsafe {
            let prev_ra_ptr = (cur_fp as *const usize).wrapping_sub(1);
            let prev_fp_ptr = (cur_fp as *const usize).wrapping_sub(2);
            let prev_ra = core::ptr::read(prev_ra_ptr);
            let prev_fp = core::ptr::read(prev_fp_ptr);
            if prev_ra == 0 || prev_fp == 0 {
                break;
            }
            error!("  #[{}] RA={:#x} FP={:#x}", depth, prev_ra, prev_fp);
            // 防止死循环
            if prev_fp == cur_fp {
                break;
            }
            cur_fp = prev_fp;
        }
    }
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
