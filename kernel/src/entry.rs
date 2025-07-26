use core::arch::naked_asm;
use crate::memory::config::KERNEL_STACK_SIZE;

/// 主CPU入口点 - 由bootloader调用
#[unsafe(naked)]
#[unsafe(no_mangle)]
#[unsafe(link_section = ".text.entry")]
unsafe extern "C" fn _start() -> ! {
    naked_asm!(
        ".option arch, +m",
        "
            # 获取hart ID到t0
            # 在S-mode下，mhartid可能不可用，使用bootloader传递的参数
            mv t0, x10      # hart_id from bootloader parameter
            
            # 计算当前CPU的栈顶地址
            # stack_top = boot_stack_top - (hart_id * KERNEL_STACK_SIZE)
            la sp, boot_stack_top
            li t1, {stack_size}
            mul t2, t0, t1
            sub sp, sp, t2
            
            # 保存参数
            add t3, x10, x0  # hart_id
            add t4, x11, x0  # dtb_addr
            
            # 只有hart 0清理BSS段
            bnez t0, 2f
            call {clear_bss}
            
        2:
            # 恢复参数并调用kmain
            mv x10, t3
            mv x11, t4
            call kmain
            
        1:
            wfi
            j 1b
        ",
        clear_bss = sym clear_bss,
        stack_size = const KERNEL_STACK_SIZE,
    )
}

/// 次要CPU入口点 - 由SBI hart_start调用
#[unsafe(naked)]
#[unsafe(no_mangle)]
unsafe extern "C" fn _secondary_start() -> ! {
    naked_asm!(
        ".option arch, +m",
        "
            # 获取hart ID到t0
            # 使用SBI传递的hart_id参数
            mv t0, x10      # hart_id from SBI parameter
            
            # 计算当前CPU的栈顶地址
            # stack_top = boot_stack_top - (hart_id * KERNEL_STACK_SIZE)
            la sp, boot_stack_top
            li t1, {stack_size}
            mul t2, t0, t1
            sub sp, sp, t2
            
            # SBI hart_start传递的参数：
            # a0 (x10) = hart_id (已由SBI设置)
            # a1 (x11) = start_addr (我们的函数地址)
            # a2 (x12) = opaque (DTB地址)
            
            # 重新设置参数为secondary_cpu_main期望的格式
            mv x10, t0      # hart_id (保持原值)
            mv x11, x12     # dtb_addr from opaque parameter
            
            # 在调用之前检查栈指针是否合理
            # 简单的完整性检查：确保栈指针在合理范围内
            la t3, boot_stack_bottom
            la t4, boot_stack_top
            bgeu sp, t3, 2f    # sp >= boot_stack_bottom
            j 3f               # 栈指针太低，无限循环
        2:
            bleu sp, t4, 4f    # sp <= boot_stack_top  
            j 3f               # 栈指针太高，无限循环
        3:
            # 栈指针无效，进入死循环
            wfi
            j 3b
        4:
            # 栈指针有效，继续执行
            # 调用secondary CPU main函数
            call {secondary_cpu_main}
            
        1:
            wfi
            j 1b
        ",
        secondary_cpu_main = sym crate::smp::boot::secondary_cpu_main,
        stack_size = const KERNEL_STACK_SIZE,
    )
}

extern "C" fn clear_bss() {
    unsafe extern "C" {
        static mut sbss: u8;
        static mut ebss: u8;
    }
    unsafe {
        let start = sbss as *const u8 as usize;
        let end = ebss as *const u8 as usize;
        let count = end - start;
        if count > 0 {
            core::ptr::write_bytes(sbss as *mut u8, 0, count);
        }
    }
}
