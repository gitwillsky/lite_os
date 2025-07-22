#![no_std]
#![no_main]

#[macro_use]
extern crate alloc;
#[macro_use]
extern crate user_lib;

use alloc::string::String;
use user_lib::{exit, get_args, mkdir};

#[unsafe(no_mangle)]
fn main() -> i32 {
    let mut argc = 0;
    let mut argv_buf = [0u8; 1024];
    
    // 获取命令行参数
    let result = get_args(&mut argc, &mut argv_buf);
    if result < 0 {
        println!("mkdir: Failed to get arguments");
        return 1;
    }
    
    // 解析参数
    if argc < 2 {
        println!("mkdir: missing operand");
        println!("Usage: mkdir <directory> [directory2] ...");
        return 1;
    }
    
    let args_str = core::str::from_utf8(&argv_buf[..result as usize]).unwrap_or("");
    let args: alloc::vec::Vec<&str> = args_str.split('\0').filter(|s| !s.is_empty()).collect();
    
    let mut exit_code = 0;
    
    // 处理每个目录参数（从第二个参数开始，第一个是程序名）
    for i in 1..args.len() {
        let path = args[i];
        let result = mkdir(path);
        
        match result {
            0 => {}, // 成功，不输出信息
            -17 => {
                println!("mkdir: cannot create directory '{}': File exists", path);
                exit_code = 1;
            },
            -13 => {
                println!("mkdir: cannot create directory '{}': Permission denied", path);
                exit_code = 1;
            },
            -2 => {
                println!("mkdir: cannot create directory '{}': No such file or directory", path);
                exit_code = 1;
            },
            -20 => {
                println!("mkdir: cannot create directory '{}': Not a directory", path);
                exit_code = 1;
            },
            -28 => {
                println!("mkdir: cannot create directory '{}': No space left on device", path);
                exit_code = 1;
            },
            _ => {
                println!("mkdir: cannot create directory '{}': Unknown error ({})", path, result);
                exit_code = 1;
            }
        }
    }
    
    exit_code
}