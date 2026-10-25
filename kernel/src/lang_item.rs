use core::panic::PanicInfo;

#[panic_handler]
fn panic_handler(info: &PanicInfo) -> ! {
    // 现场保存、蓝屏与停机/重启策略全部由 crash 模块拥有；panic 永不返回。
    crate::crash::handle_panic(info)
}
