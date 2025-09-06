#![no_std]
#![no_main]

#[macro_use]
extern crate alloc;
#[macro_use]
extern crate user_lib;

use user_lib::{exit, getcwd};

#[unsafe(no_mangle)]
fn main() -> i32 {
    let mut buf = [0u8; 256];
    let result = getcwd(&mut buf);

    if result > 0 {
        // Find the null terminator or use the returned length
        let len = result as usize - 1; // Subtract 1 for null terminator
        if let Ok(cwd) = core::str::from_utf8(&buf[..len]) {
            println!("{}", cwd);
            0
        } else {
            println!("pwd: Invalid UTF-8 in current directory path");
            1
        }
    } else {
        println!("pwd: Cannot get current directory");
        1
    }
}
