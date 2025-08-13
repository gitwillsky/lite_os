use core::panic::PanicInfo;
use riscv::register;

#[panic_handler]
fn panic_handler(info: &PanicInfo) -> ! {
    unsafe { register::sstatus::clear_sie(); }

    // 尽量使用 panic 直写控制台，避免拿锁
    if let Some(location) = info.location() {
        crate::console::panic_println_fmt(format_args!(
            "BUG: kernel panic: {}\n      at: {}:{}:{}\n      cpu: {}",
            info.message(),
            location.file(),
            location.line(),
            location.column(),
            crate::arch::hart::hart_id()
        ));
    } else {
        crate::console::panic_println_fmt(format_args!(
            "BUG: kernel panic: {}\n      cpu: {}",
            info.message(),
            crate::arch::hart::hart_id()
        ));
    }

    // 进入全核冻结与上下文采集
    crate::trap::crashdump::leader_freeze_and_collect()
}
