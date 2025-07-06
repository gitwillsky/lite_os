#![no_std]
#![no_main]

#[macro_use]
extern crate alloc;
#[macro_use]
extern crate user_lib;

use alloc::string::String;
use user_lib::{exec, fork, read, wait_pid, yield_};

const LF: u8 = b'\n';
const CR: u8 = b'\r';
const DL: u8 = b'\x7f'; // DEL
const BS: u8 = b'\x08'; // BACKSPACE

fn get_char() -> u8 {
    let mut byte = [0u8; 1];
    if read(0, &mut byte) <= 0 {
        return 0;
    }
    byte[0]
}

fn read_line(buf: &mut [u8]) -> usize {
    let mut i = 0;
    while i < buf.len() {
        let mut byte = [0u8; 1];
        if read(0, &mut byte) <= 0 {
            // 如果没有输入，可以稍微等待一下，避免CPU空转
            // 在更高级的实现中，这里应该是阻塞或yield
            continue;
        }

        let c = byte[0];
        match c {
            CR | LF => {
                print!("\n");
                break;
            }
            BS | DL => {
                if i > 0 {
                    i -= 1;
                    // 在控制台上实现退格效果
                    print!("\x08 \x08");
                }
            }
            _ => {
                buf[i] = c;
                i += 1;
                print!("{}", c as char);
            }
        }
    }
    i
}

#[unsafe(no_mangle)]
fn main() -> i32 {
    let mut line: String = String::new();
    print!("$");
    loop {
        let c = get_char();
        match c {
            0 => {
                yield_();
                continue;
            }
            CR | LF => {
                println!("");
                if !line.is_empty() {
                    line.push('\0');
                    let pid = fork();
                    if pid == 0 {
                        if exec(line.as_str()) == -1 {
                            println!("command not found: {}", line);
                        }
                    } else {
                        let mut exit_code: i32 = 0;
                        let exit_pid = wait_pid(pid as usize, &mut exit_code);
                        assert_eq!(pid, exit_pid);
                        if exit_code != 0 {
                            println!("Shell: Process {} exited with code {}", pid, exit_code);
                        }
                    }
                    line.clear();
                }
                print!("$");
            }
            BS | DL => {
                if line.len() > 0 {
                    print!("{}", BS as char);
                    print!("{}", ' ' as char);
                    print!("{}", BS as char);
                    line.pop();
                }
            }
            _ => {
                print!("{}", c as char);
                line.push(c as char);
            }
        }
    }
    0
}
