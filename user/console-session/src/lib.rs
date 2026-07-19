#![no_std]
#![no_main]

//! Single-process graphical console for standard Linux TUI applications.
//!
//! # Safety model
//!
//! 1. The reactor is the sole owner of device and PTY file descriptors; every FFI buffer remains
//!    live for the complete syscall and is sized from the corresponding Linux UAPI structure.
//! 2. `Model` is the sole owner of its `calloc` grids. Checked dimensions establish every raw-cell
//!    access bound, resize candidates transfer ownership once, and `Drop` frees each allocation.
//! 3. `Display` is the sole owner of each GEM mapping and framebuffer. Rendering clips coordinates
//!    to the checked mapping geometry before forming slices, and teardown unmaps before closing DRM.
//! 4. Any violated syscall, allocation, ownership or commit invariant terminates the session; init
//!    reconstructs it from kernel-owned device and PTY state instead of continuing with partial state.

mod atlas;
mod display;
mod ffi;
mod model;
mod reactor;

use core::{ffi::c_int, panic::PanicInfo};

#[unsafe(no_mangle)]
pub extern "C" fn main(_argument_count: c_int, _arguments: *const *const u8) -> c_int {
    let mut reported = false;
    loop {
        match reactor::run() {
            Ok(()) => return 0,
            Err(()) => {
                if !reported {
                    let message = b"console-session: unavailable; retrying\n";
                    unsafe { ffi::write(2, message.as_ptr().cast(), message.len()) };
                    reported = true;
                }
                // Headless boots intentionally lack DRM/input. Keeping the same process alive
                // prevents init's respawn policy from turning absence into an exec/I/O storm;
                // retrying still lets a later device/session reconstruction use the sole reactor.
                unsafe { ffi::poll(core::ptr::null_mut(), 0, 5_000) };
            }
        }
    }
}

#[panic_handler]
fn panic(_information: &PanicInfo<'_>) -> ! {
    let message = b"console-session: invariant failure\n";
    unsafe {
        ffi::write(2, message.as_ptr().cast(), message.len());
        ffi::_exit(125)
    }
}
