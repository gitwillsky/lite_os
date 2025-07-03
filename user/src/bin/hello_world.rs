#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;

use user_lib::sys_write;

#[unsafe(no_mangle)]
fn main() -> i32 {
    // 直接使用系统调用输出简单字符串
    let hello_msg = b"[user] Hello from user program!\n";
    sys_write(1, hello_msg);

    let exit_msg = b"[user] Program exiting\n";
    sys_write(1, exit_msg);

    0
}
