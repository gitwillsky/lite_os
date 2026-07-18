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
    match reactor::run() {
        Ok(()) => 0,
        Err(()) => {
            let message = b"console-session: session failed\n";
            unsafe { ffi::write(2, message.as_ptr().cast(), message.len()) };
            1
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
