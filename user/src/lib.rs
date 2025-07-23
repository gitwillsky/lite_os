#![no_std]
#![feature(linkage)]
#![feature(alloc_error_handler)]

pub mod syscall;
#[macro_use]
pub mod console;
pub mod heap;

mod lang_item;

#[macro_use]
extern crate alloc;

pub use syscall::*;


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
