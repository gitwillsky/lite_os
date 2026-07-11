use core::panic::PanicInfo;
use riscv::register;

#[panic_handler]
fn panic_handler(info: &PanicInfo) -> ! {
    // SAFETY: panic handling runs in S-mode and disables only the current hart's interrupt bit
    // before entering a non-returning diagnostic path.
    unsafe {
        register::sstatus::clear_sie();
    }

    // 输出基本的 panic 信息
    if let Some(location) = info.location() {
        crate::arch::console::panic_println_fmt(format_args!(
            "KERNEL PANIC: {}\n  at {}:{}:{}\n  CPU: {}",
            info.message(),
            location.file(),
            location.line(),
            location.column(),
            crate::arch::hart::raw_hart_id()
        ));
    } else {
        crate::arch::console::panic_println_fmt(format_args!(
            "KERNEL PANIC: {}\n  CPU: {}",
            info.message(),
            crate::arch::hart::raw_hart_id()
        ));
    }

    // 1. SRST 是整个 SMP 系统的 fail-stop 路径；仅停住当前 hart 会让其他 hart
    // 在全局不变量已经失效后继续修改共享状态。
    let _ = crate::arch::sbi::system_reset(0, 1);

    // 2. firmware 不支持或错误返回时，本 hart 保持中断关闭并永久停机。
    loop {
        riscv::asm::wfi();
    }
}
