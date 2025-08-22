#![no_std]
#![no_main]

#[macro_use]
extern crate alloc;
#[macro_use]
extern crate user_lib;

use alloc::string::String;
use user_lib::{exit, get_args};

#[unsafe(no_mangle)]
fn main() -> i32 {
    let mut argc = 0;
    let mut argv_buf = [0u8; 1024];

    // 获取命令行参数
    let result = get_args(&mut argc, &mut argv_buf);
    if result < 0 {
        return 1;
    }

    if argc <= 1 {
        // 没有参数时，只输出换行
        println!("");
        return 0;
    }

    let args_str = core::str::from_utf8(&argv_buf[..result as usize]).unwrap_or("");
    let args: alloc::vec::Vec<&str> = args_str.split('\0').filter(|s| !s.is_empty()).collect();

    // 输出所有参数（从第二个参数开始，第一个是程序名）
    for i in 1..args.len() {
        if i > 1 {
            print!(" "); // 参数间用空格分隔
        }
        print!("{}", args[i]);
    }
    println!(""); // 最后输出换行

    0
}
