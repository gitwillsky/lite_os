#![no_std]
#![no_main]

use user_lib::*;

#[unsafe(no_mangle)]
pub fn main() -> i32 {
    println!("Testing file system...");
    
    // 测试列出根目录
    let mut buf = [0u8; 1024];
    let len = listdir("/", &mut buf);
    if len >= 0 {
        println!("Root directory contents:");
        let contents = core::str::from_utf8(&buf[..len as usize]).unwrap_or("Invalid UTF-8");
        println!("{}", contents);
    } else {
        println!("Failed to list root directory");
    }
    
    // 测试读取文件
    let mut file_buf = [0u8; 512];
    let file_len = read_file("/hello.txt", &mut file_buf);
    if file_len >= 0 {
        println!("File contents:");
        let contents = core::str::from_utf8(&file_buf[..file_len as usize]).unwrap_or("Invalid UTF-8");
        println!("{}", contents);
    } else {
        println!("Failed to read file /hello.txt");
    }
    
    println!("File system test completed!");
    0
}