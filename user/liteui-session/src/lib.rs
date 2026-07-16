#![no_std]
#![no_main]

mod ffi;
mod listener;
mod process;
mod supervisor;

use core::{ffi::c_int, panic::PanicInfo};

#[unsafe(no_mangle)]
pub extern "C" fn main(_argument_count: c_int, _arguments: *const *const u8) -> c_int {
    match supervisor::run() {
        Ok(()) => 0,
        Err(()) => {
            ffi::write_stderr(
                b"liteui-session: session unavailable; UART recovery remains active\n",
            );
            125
        }
    }
}

#[panic_handler]
fn panic(_information: &PanicInfo<'_>) -> ! {
    ffi::write_stderr(b"liteui-session: invariant failure\n");
    unsafe { ffi::_exit(125) }
}
