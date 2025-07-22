#![no_std]
#![no_main]

#[macro_use]
extern crate alloc;
#[macro_use]
extern crate user_lib;

use alloc::string::String;
use user_lib::{exit, get_args, remove};

#[unsafe(no_mangle)]
fn main() -> i32 {
    let mut argc = 0;
    let mut argv_buf = [0u8; 1024];
    
    // 获取命令行参数
    let result = get_args(&mut argc, &mut argv_buf);
    if result < 0 {
        println!("rm: Failed to get arguments");
        return 1;
    }
    
    // 解析参数
    if argc < 2 {
        println!("rm: missing operand");
        println!("Usage: rm <file> [file2] ...");
        return 1;
    }
    
    let args_str = core::str::from_utf8(&argv_buf[..result as usize]).unwrap_or("");
    let args: alloc::vec::Vec<&str> = args_str.split('\0').filter(|s| !s.is_empty()).collect();
    
    let mut exit_code = 0;
    
    // 处理每个文件参数（从第二个参数开始，第一个是程序名）
    for i in 1..args.len() {
        let path = args[i];
        let result = remove(path);
        
        if result == 0 {
            // 成功删除，不输出信息（类似标准rm行为）
        } else {
            match result {
                -2 => println!("rm: cannot remove '{}': No such file or directory", path),
                -13 => println!("rm: cannot remove '{}': Permission denied", path),
                -16 => println!("rm: cannot remove '{}': Device or resource busy", path),
                -21 => println!("rm: cannot remove '{}': Is a directory", path),
                -39 => println!("rm: cannot remove '{}': Directory not empty", path),
                _ => println!("rm: cannot remove '{}': Unknown error ({})", path, result),
            }
            exit_code = 1;
        }
    }
    
    exit_code
}