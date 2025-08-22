use core::panic::PanicInfo;
use riscv::register;

#[panic_handler]
fn panic_handler(info: &PanicInfo) -> ! {
    unsafe {
        register::sstatus::clear_sie();
    }

    // 输出基本的 panic 信息
    if let Some(location) = info.location() {
        crate::console::panic_println_fmt(format_args!(
            "KERNEL PANIC: {}\n  at {}:{}:{}\n  CPU: {}",
            info.message(),
            location.file(),
            location.line(),
            location.column(),
            crate::arch::hart::hart_id()
        ));
    } else {
        crate::console::panic_println_fmt(format_args!(
            "KERNEL PANIC: {}\n  CPU: {}",
            info.message(),
            crate::arch::hart::hart_id()
        ));
    }

    // 简单停机
    loop {
        unsafe {
            riscv::asm::wfi();
        }
    }
}
