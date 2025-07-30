use core::arch::naked_asm;

#[unsafe(naked)]
#[unsafe(no_mangle)]
#[unsafe(link_section = ".text.entry")]
unsafe extern "C" fn _start() -> ! {
    naked_asm!(
        "
            # 获取hart_id (在a0寄存器中)
            # 计算每个核心的栈顶地址: boot_stack_top - hart_id * KERNEL_STACK_SIZE
            # 使用移位代替乘法: KERNEL_STACK_SIZE = 16KB = 16384 = 1 << 14
            slli t1, a0, 14         # t1 = hart_id << 14 = hart_id * 16384
            la sp, boot_stack_top   # sp = boot_stack_top
            sub sp, sp, t1          # sp = boot_stack_top - hart_id * KERNEL_STACK_SIZE

            # 保存参数
            add t0, x10, x0         # hart_id
            add t1, x11, x0         # dtb_addr

            # 保存BSS清理状态到全局变量，让第一个核心清理
            # 这里简化处理：总是调用clear_bss，函数内部会处理重复调用
            call {clear_bss}
        2:
            # 恢复参数并调用kmain
            mv x10, t0
            mv x11, t1
            call kmain
        1:
            wfi
            j 1b
        ",
        clear_bss = sym clear_bss,
    )
}

extern "C" fn clear_bss() {
    use core::sync::atomic::{AtomicBool, Ordering};

    // 全局标志，确保BSS只被清理一次
    static BSS_CLEARED: AtomicBool = AtomicBool::new(false);

    // 只有第一个调用的核心才执行BSS清理
    if BSS_CLEARED.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_ok() {
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
}
