#![no_std]
#![no_main]

use user_lib::*;

/// Simple test program that demonstrates it received execution
#[unsafe(no_mangle)]
fn main() -> i32 {
    let mut argc_buf = 0usize;
    let mut argv_buf = [0u8; 1024];
    get_args(&mut argc_buf, &mut argv_buf);
    println!("Arguments Test Program - RUNNING");
    println!("================================");

    // 解析并打印字符串参数
    if argc_buf > 0 {
        let mut offset = 0;

        for i in 0..argc_buf {
            if offset >= argv_buf.len() {
                break;
            }

            // 找到下一个null终止符
            let arg_end = argv_buf[offset..]
                .iter()
                .position(|&x| x == 0)
                .unwrap_or(argv_buf.len() - offset);

            if arg_end > 0 {
                if let Ok(arg_str) = core::str::from_utf8(&argv_buf[offset..offset + arg_end]) {
                    println!("  argv[{}] = \"{}\"", i, arg_str);
                } else {
                    println!("  argv[{}] = <invalid utf8>", i);
                }
            } else {
                println!("  argv[{}] = <empty>", i);
            }

            offset += arg_end + 1; // +1 for null terminator
        }
    } else {
        println!("No arguments received");
    }

    println!("This program was successfully executed!");
    println!("The argument passing mechanism is working.");

    println!("Program completed successfully!");
    0
}