#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;


#[unsafe(no_mangle)]
fn main() -> i32 {
    // 直接使用系统调用输出简单字符串
    println!("[user] Hello from user program!");

    println!("[user] Program exiting");
    0
}
