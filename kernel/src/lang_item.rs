use core::panic::PanicInfo;

#[panic_handler]
fn panic_handler(info: &PanicInfo) -> ! {
    // SAFETY: panic handling disables only the current CPU's local interrupt state before entering
    // a non-returning diagnostic path.
    crate::arch::interrupt::disable_for_fail_stop();

    // 输出基本的 panic 信息
    if let Some(location) = info.location() {
        crate::platform::console::panic_println_fmt(format_args!(
            "KERNEL PANIC: {}\n  at {}:{}:{}\n  CPU: {:?}",
            info.message(),
            location.file(),
            location.line(),
            location.column(),
            crate::cpu::executing_hardware_id()
        ));
    } else {
        crate::platform::console::panic_println_fmt(format_args!(
            "KERNEL PANIC: {}\n  CPU: {:?}",
            info.message(),
            crate::cpu::executing_hardware_id()
        ));
    }

    // 1. platform reset 是整个 SMP 系统的 fail-stop 路径；仅停住当前 CPU 会让其他 CPU
    // 在全局不变量已经失效后继续修改共享状态。
    let _ = crate::platform::reset_system(0, 1);

    // 2. firmware 不支持或错误返回时，当前 CPU 保持中断关闭并永久停机。
    loop {
        crate::arch::interrupt::wait();
    }
}
