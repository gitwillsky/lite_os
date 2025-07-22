#![no_std]
#![no_main]

#[macro_use]
extern crate alloc;
#[macro_use]
extern crate user_lib;

use alloc::string::String;
use user_lib::{exit, get_args, read_file};

#[unsafe(no_mangle)]
fn main() -> i32 {
    let mut argc = 0;
    let mut argv_buf = [0u8; 1024];
    
    // 获取命令行参数
    let result = get_args(&mut argc, &mut argv_buf);
    if result < 0 {
        println!("cat: Failed to get arguments");
        return 1;
    }
    
    // 解析参数
    if argc < 2 {
        println!("cat: missing file operand");
        println!("Usage: cat <file> [file2] ...");
        return 1;
    }
    
    let args_str = core::str::from_utf8(&argv_buf[..result as usize]).unwrap_or("");
    let args: alloc::vec::Vec<&str> = args_str.split('\0').filter(|s| !s.is_empty()).collect();
    
    let mut exit_code = 0;
    
    // 处理每个文件参数（从第二个参数开始，第一个是程序名）
    for i in 1..args.len() {
        let path = args[i];
        let mut buf = [0u8; 4096];
        let len = read_file(path, &mut buf);
        
        if len >= 0 {
            let contents = core::str::from_utf8(&buf[..len as usize]).unwrap_or("Invalid UTF-8");
            print!("{}", contents);
        } else {
            match len {
                -2 => println!("cat: {}: No such file or directory", path),
                -13 => println!("cat: {}: Permission denied", path),
                -21 => println!("cat: {}: Is a directory", path),
                _ => println!("cat: {}: Unknown error ({})", path, len),
            }
            exit_code = 1;
        }
    }
    
    exit_code
}