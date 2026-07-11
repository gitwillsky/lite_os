use core::panic::PanicInfo;

use crate::exit_group;

#[panic_handler]
fn panic_handler(info: &PanicInfo) -> ! {
    if let Some(location) = info.location() {
        println!(
            "[User] Panic at {}:{}:{} {}",
            location.file(),
            location.line(),
            location.column(),
            info.message()
        );
    } else {
        println!("[User] Panic: {}", info.message());
    }
    exit_group(127)
}
