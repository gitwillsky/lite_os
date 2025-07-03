#![no_std]
#![feature(linkage)]

pub mod syscall;
#[macro_use]
pub mod console;
mod lang_item;

use syscall::*;

pub use syscall::sys_read;
pub use syscall::sys_write;

#[unsafe(no_mangle)]
#[unsafe(link_section = ".text.entry")]
extern "C" fn _start() -> ! {
    exit(main());
    unreachable!()
}

#[linkage = "weak"] // 弱符号，如果用户没有提供 main 函数，则使用这个默认的
#[unsafe(no_mangle)]
fn main() -> i32 {
    panic!("Can not find app main function")
}

pub fn write(fd: usize, buf: &[u8]) -> isize {
    sys_write(fd, buf)
}

pub fn exit(code: i32) -> isize {
    sys_exit(code)
}
