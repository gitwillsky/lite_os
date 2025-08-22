#![no_std]
#![feature(linkage)]
#![feature(alloc_error_handler)]

pub mod syscall;
#[macro_use]
pub mod console;
pub mod gfx;
pub mod heap;
pub mod litegui;
pub mod protocol;
pub mod testutil;
pub mod webcore;

mod lang_item;

extern crate alloc;

pub use syscall::*;
pub use testutil::TestStats;

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

// 检查键盘输入
pub fn check_keyboard_input(nonblock: bool) -> Option<u8> {
    use crate::syscall::{errno, fcntl_getfl, fcntl_setfl, open_flags};

    let current_flags = fcntl_getfl(0);
    if current_flags >= 0 {
        let mut flags = current_flags as u32;
        if nonblock {
            flags |= open_flags::O_NONBLOCK;
        } else {
            flags &= !open_flags::O_NONBLOCK;
        }
        if flags != current_flags as u32 {
            fcntl_setfl(0, flags);
        }
    }

    let mut buffer = [0u8; 1];
    match read(0, &mut buffer) {
        1 => Some(buffer[0]),
        err if err == -(errno::EAGAIN as isize) => None,
        _ => None,
    }
}
