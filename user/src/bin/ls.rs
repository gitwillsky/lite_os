#![no_std]
#![no_main]

#[macro_use]
extern crate alloc;
#[macro_use]
extern crate user_lib;

use alloc::string::String;
use user_lib::{exit, get_args, listdir, getcwd};

#[unsafe(no_mangle)]
fn main() -> i32 {
    let mut argc = 0;
    let mut argv_buf = [0u8; 1024];
    
    // 获取命令行参数
    let result = get_args(&mut argc, &mut argv_buf);
    if result < 0 {
        println!("ls: Failed to get arguments");
        return 1;
    }
    
    // 解析参数
    let mut path = "."; // 默认为当前目录
    if argc > 1 {
        // 简单解析：找到第一个参数（跳过程序名）
        let args_str = core::str::from_utf8(&argv_buf[..result as usize]).unwrap_or("");
        let args: alloc::vec::Vec<&str> = args_str.split('\0').filter(|s| !s.is_empty()).collect();
        if args.len() > 1 {
            path = args[1];
        }
    }
    
    // 执行ls操作
    let mut buf = [0u8; 1024];
    let len = listdir(path, &mut buf);
    if len >= 0 {
        let contents = core::str::from_utf8(&buf[..len as usize]).unwrap_or("Invalid UTF-8");
        print!("{}", contents);
        0
    } else {
        match len {
            -2 => println!("ls: cannot access '{}': No such file or directory", path),
            -13 => println!("ls: cannot open directory '{}': Permission denied", path),
            -20 => println!("ls: cannot access '{}': Not a directory", path),
            _ => println!("ls: cannot access '{}': Unknown error ({})", path, len),
        }
        1
    }
}