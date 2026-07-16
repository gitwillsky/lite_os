#![no_std]
#![no_main]

mod display;
mod ffi;
mod input;
mod reactor;
mod scene;

use core::{ffi::c_int, panic::PanicInfo};

#[unsafe(no_mangle)]
pub extern "C" fn main(_argument_count: c_int, _arguments: *const *const u8) -> c_int {
    match reactor::run() {
        Ok(()) => 0,
        Err(()) => {
            let message = b"liteos-2d: display session failed\n";
            unsafe { ffi::write(2, message.as_ptr().cast(), message.len()) };
            1
        }
    }
}

#[panic_handler]
fn panic(_information: &PanicInfo<'_>) -> ! {
    let message = b"liteos-2d: invariant failure\n";
    unsafe {
        ffi::write(2, message.as_ptr().cast(), message.len());
        ffi::_exit(125)
    }
}
