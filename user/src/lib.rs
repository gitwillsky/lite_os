#![no_std]
#![feature(linkage)]

pub mod syscall;
#[macro_use]
pub mod console;
mod lang_item;

use syscall::*;

pub use syscall::sys_read;

#[unsafe(no_mangle)]
#[unsafe(link_section = ".text.entry")]
extern "C" fn _start() -> ! {
    clear_bss();
    exit(main());
    unreachable!()
}

#[linkage = "weak"] // 弱符号，如果用户没有提供 main 函数，则使用这个默认的
#[unsafe(no_mangle)]
fn main() -> i32 {
    panic!("Can not find app main function")
}

fn clear_bss() {
    unsafe extern "C" {
        static mut sbss: u8;
        static mut ebss: u8;
    }
    unsafe {
        let bss_start = sbss as *const u8 as usize;
        let bss_end = ebss as *const u8 as usize;
        let count = bss_end - bss_start;
        if count > 0 {
            core::ptr::write_bytes(bss_start as *mut u8, 0, count);
        }
    }
}

pub fn write(fd: usize, buf: &[u8]) -> isize {
    sys_write(fd, buf)
}

pub fn exit(code: i32) -> isize {
    sys_exit(code)
}
