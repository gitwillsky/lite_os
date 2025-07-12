#![no_std]
#![no_main]

#[macro_use]
extern crate alloc;
#[macro_use]
extern crate user_lib;

use alloc::string::String;
use alloc::vec::Vec;
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
                    // 处理内置命令
                    if line.starts_with("ls") {
                        handle_ls_command(&line);
                    } else if line.starts_with("cat") {
                        handle_cat_command(&line);
                    } else if line.starts_with("mkdir") {
                        handle_mkdir_command(&line);
                    } else if line.starts_with("rm") {
                        handle_rm_command(&line);
                    } else {
                        // 执行外部程序
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

fn handle_ls_command(line: &str) {
    let path = if line.len() > 2 {
        line[2..].trim()
    } else {
        "/"
    };
    
    let mut buf = [0u8; 1024];
    let len = user_lib::listdir(path, &mut buf);
    if len >= 0 {
        let contents = core::str::from_utf8(&buf[..len as usize]).unwrap_or("Invalid UTF-8");
        print!("{}", contents);
    } else {
        println!("ls: cannot access '{}': No such file or directory", path);
    }
}

fn handle_cat_command(line: &str) {
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() < 2 {
        println!("cat: missing file operand");
        return;
    }
    
    let path = parts[1];
    let mut buf = [0u8; 4096];
    let len = user_lib::read_file(path, &mut buf);
    if len >= 0 {
        let contents = core::str::from_utf8(&buf[..len as usize]).unwrap_or("Invalid UTF-8");
        print!("{}", contents);
    } else {
        println!("cat: {}: No such file or directory", path);
    }
}

fn handle_mkdir_command(line: &str) {
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() < 2 {
        println!("mkdir: missing operand");
        return;
    }
    
    let path = parts[1];
    let result = user_lib::mkdir(path);
    match result {
        0 => println!("Directory '{}' created", path),
        -17 => println!("mkdir: cannot create directory '{}': File exists", path),
        -13 => println!("mkdir: cannot create directory '{}': Permission denied", path),
        -2 => println!("mkdir: cannot create directory '{}': No such file or directory", path),
        -20 => println!("mkdir: cannot create directory '{}': Not a directory", path),
        -28 => println!("mkdir: cannot create directory '{}': No space left on device", path),
        _ => println!("mkdir: cannot create directory '{}': Unknown error ({})", path, result),
    }
}

fn handle_rm_command(line: &str) {
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() < 2 {
        println!("rm: missing operand");
        return;
    }
    
    let path = parts[1];
    if user_lib::remove(path) == 0 {
        println!("'{}' removed", path);
    } else {
        println!("rm: cannot remove '{}': No such file or directory", path);
    }
}
