#![no_std]
#![no_main]
#![feature(alloc_error_handler)]

extern crate alloc;

mod broker;
mod client;
mod ffi;
mod peer;
mod protocol;

use core::{alloc::Layout, ffi::c_int, panic::PanicInfo};

struct MuslAllocator;

unsafe impl core::alloc::GlobalAlloc for MuslAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        unsafe { ffi::malloc(layout.size().max(1)).cast() }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, _layout: Layout) {
        unsafe { ffi::free(pointer.cast()) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, _layout: Layout, size: usize) -> *mut u8 {
        unsafe { ffi::realloc(pointer.cast(), size.max(1)).cast() }
    }
}

#[global_allocator]
static ALLOCATOR: MuslAllocator = MuslAllocator;

#[unsafe(no_mangle)]
pub extern "C" fn main(_argument_count: c_int, _arguments: *const *const u8) -> c_int {
    match broker::run() {
        Ok(()) => 0,
        Err(()) => {
            ffi::write_stderr(b"display-session: fatal session invariant\n");
            125
        }
    }
}

#[alloc_error_handler]
fn allocation_failure(_layout: Layout) -> ! {
    ffi::write_stderr(b"display-session: allocation failure\n");
    unsafe { ffi::_exit(125) }
}

#[panic_handler]
fn panic(_information: &PanicInfo<'_>) -> ! {
    ffi::write_stderr(b"display-session: invariant failure\n");
    unsafe { ffi::_exit(125) }
}
