use core::{
    panic::PanicInfo,
    sync::atomic::{AtomicBool, Ordering},
};

use riscv::register;

use crate::arch::sbi;

static IN_PANIC: AtomicBool = AtomicBool::new(false);
#[panic_handler]
fn panic_handler(info: &PanicInfo) -> ! {
    if IN_PANIC.swap(true, Ordering::SeqCst) {
        loop {
            riscv::asm::wfi();
        }
    }

    // 禁用中断
    unsafe {
        register::sstatus::clear_sie();
    }

    if let Some(location) = info.location() {
        println!(
            "[Kernel] Panic at {}:{}:{} {}",
            location.file(),
            location.line(),
            location.column(),
            info.message()
        );
    } else {
        println!("[Kernel] Panic: {}", info.message());
    }

    _ = sbi::shutdown();

    #[allow(unreachable_code)]
    loop {
        riscv::asm::wfi();
    }
}
