use core::{
    panic::PanicInfo,
    sync::atomic::{AtomicBool, Ordering},
};

use riscv::register;

use crate::arch::sbi;
use crate::debug;

static IN_PANIC: AtomicBool = AtomicBool::new(false);

/// 简单的堆栈回溯实现
fn print_stack_trace() {
    error!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    error!("                    KERNEL STACK TRACE");
    error!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    
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
    error!("");
    error!("Call Stack:");
    
    let mut depth = 0;
    const MAX_DEPTH: usize = 15;
    
    // 首先打印当前的返回地址
    if ra != 0 {
        error!("  #{}: {}", depth, debug::format_address(ra));
        depth += 1;
    }
    
    // 进行栈回溯
    while depth < MAX_DEPTH && fp != 0 {
        // 检查帧指针是否在合理范围内
        if fp < 0x80000000 || fp >= 0x90000000 {
            break;
        }
        
        // 确保对齐
        if fp % 8 != 0 {
            break;
        }
        
        // 尝试安全地读取栈帧数据
        let (saved_ra, saved_fp) = unsafe {
            // 在 RISC-V 中，标准的栈帧布局：
            // fp-8: 保存的 ra (返回地址)  
            // fp-16: 保存的 fp (上一个帧指针)
            
            // 使用更保守的方法读取内存
            let ra_ptr = (fp - 8) as *const usize;
            let fp_ptr = (fp - 16) as *const usize;
            
            // 检查指针是否在合理范围内
            let saved_ra = if fp >= 16 && ra_ptr as usize >= 0x80000000 {
                core::ptr::read_volatile(ra_ptr)
            } else {
                0
            };
            
            let saved_fp = if fp >= 16 && fp_ptr as usize >= 0x80000000 {
                core::ptr::read_volatile(fp_ptr)
            } else {
                0
            };
            
            (saved_ra, saved_fp)
        };
        
        // 验证返回地址是否合理
        if saved_ra > 0x80000000 && saved_ra < 0x90000000 {
            error!("  #{}: {}", depth, debug::format_address(saved_ra));
        }
        
        // 验证帧指针并防止无限循环
        if saved_fp == 0 || saved_fp <= fp || saved_fp >= 0x90000000 {
            break;
        }
        
        fp = saved_fp;
        
        depth += 1;
    }
    
    if depth <= 1 {
        error!("  (limited stack trace - frame pointers may not be available)");
        
        // 作为备选方案，尝试打印栈内容
        error!("");
        error!("Stack Memory Analysis:");
        unsafe {
            for i in 0..8 {
                let addr = sp + (i * 8);
                if addr >= 0x80000000 && addr < 0x90000000 {
                    let value = core::ptr::read_volatile(addr as *const usize);
                    if value > 0x80000000 && value < 0x90000000 {
                        error!("  SP+{:#x}: {}", i * 8, debug::format_address(value));
                    }
                }
            }
        }
    }
    
    error!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
}
#[panic_handler]
fn panic_handler(info: &PanicInfo) -> ! {
    if IN_PANIC.swap(true, Ordering::SeqCst) {
        loop {
            riscv::asm::wfi();
        }
    }

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
