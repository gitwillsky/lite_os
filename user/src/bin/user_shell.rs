#![no_std]
#![no_main]

#[macro_use]
extern crate alloc;
#[macro_use]
extern crate user_lib;

use alloc::string::String;
use alloc::vec::Vec;
use user_lib::{exec, fork, read, wait_pid, yield_, open, close, dup2};

const LF: u8 = b'\n';
const CR: u8 = b'\r';
const DL: u8 = b'\x7f'; // DEL
const BS: u8 = b'\x08'; // BACKSPACE
const TAB: u8 = b'\t';  // TAB

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

// 计算字符在屏幕上的显示宽度
fn char_display_width(c: char, cursor_pos: usize) -> usize {
    match c {
        '\t' => {
            // Tab stops every 8 columns
            8 - (cursor_pos % 8)
        }
        _ => 1,
    }
}

// 计算字符串在屏幕上的显示宽度
fn string_display_width(s: &str) -> usize {
    let mut width = 0;
    for c in s.chars() {
        width += char_display_width(c, width);
    }
    width
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
                    } else if line.starts_with("cd") {
                        handle_cd_command(&line);
                    } else if line.starts_with("pwd") {
                        handle_pwd_command(&line);
                    } else {
                        // 执行外部程序，支持重定向
                        execute_command_with_redirection(&line);
                    }
                    line.clear();
                }
                print!("$");
            }
            TAB => {
                // 处理Tab字符 - 扩展为空格直到下一个tab stop
                let current_pos = 1 + string_display_width(&line); // 1 for '$' prompt
                let spaces_to_add = 8 - (current_pos % 8);
                for _ in 0..spaces_to_add {
                    print!(" ");
                }
                line.push('\t');
            }
            BS | DL => {
                if line.len() > 0 {
                    let removed_char = line.pop().unwrap();
                    // 计算要删除的字符的显示宽度
                    let current_pos = 1 + string_display_width(&line); // position after removal
                    let char_width = char_display_width(removed_char, current_pos);
                    
                    // 退格删除相应数量的字符
                    for _ in 0..char_width {
                        print!("{} {}", BS as char, BS as char);
                    }
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

// 解析命令和重定向
fn parse_command_with_redirection(line: &str) -> (String, Option<String>, Option<String>) {
    let mut command = String::new();
    let mut output_file = None;
    let mut input_file = None;
    
    let parts: Vec<&str> = line.split_whitespace().collect();
    let mut i = 0;
    
    while i < parts.len() {
        match parts[i] {
            ">" => {
                // 输出重定向
                if i + 1 < parts.len() {
                    output_file = Some(String::from(parts[i + 1]));
                    i += 2;
                } else {
                    println!("shell: syntax error near unexpected token '>'");
                    return (command, None, None);
                }
            }
            "<" => {
                // 输入重定向
                if i + 1 < parts.len() {
                    input_file = Some(String::from(parts[i + 1]));
                    i += 2;
                } else {
                    println!("shell: syntax error near unexpected token '<'");
                    return (command, None, None);
                }
            }
            _ => {
                if !command.is_empty() {
                    command.push(' ');
                }
                command.push_str(parts[i]);
                i += 1;
            }
        }
    }
    
    (command, output_file, input_file)
}

// 执行带重定向的命令
fn execute_command_with_redirection(line: &str) {
    let (command, output_file, input_file) = parse_command_with_redirection(line);
    
    if command.is_empty() {
        return;
    }
    
    let mut cmd_with_null = command.clone();
    cmd_with_null.push('\0');
    
    let pid = fork();
    if pid == 0 {
        // 子进程：设置重定向并执行命令
        
        // 设置输入重定向
        if let Some(input_filename) = input_file {
            let mut input_filename_with_null = input_filename;
            input_filename_with_null.push('\0');
            let input_fd = open(input_filename_with_null.as_str(), 0);
            if input_fd < 0 {
                println!("shell: {}: No such file or directory", input_filename_with_null.trim_end_matches('\0'));
                return;
            }
            // 重定向 stdin (fd 0) 到输入文件
            if dup2(input_fd as usize, 0) < 0 {
                println!("shell: failed to redirect input");
                close(input_fd as usize);
                return;
            }
            close(input_fd as usize);
        }
        
        // 设置输出重定向
        if let Some(output_filename) = output_file {
            let mut output_filename_with_null = output_filename;
            output_filename_with_null.push('\0');
            let output_fd = open(output_filename_with_null.as_str(), 1); // Open for write
            if output_fd < 0 {
                println!("shell: failed to create output file: {}", output_filename_with_null.trim_end_matches('\0'));
                return;
            }
            // 重定向 stdout (fd 1) 到输出文件
            if dup2(output_fd as usize, 1) < 0 {
                println!("shell: failed to redirect output");
                close(output_fd as usize);
                return;
            }
            close(output_fd as usize);
        }
        
        // 执行命令
        if exec(cmd_with_null.as_str()) == -1 {
            println!("command not found: {}", command);
        }
    } else {
        // 父进程：等待子进程完成
        let mut exit_code: i32 = 0;
        let exit_pid = wait_pid(pid as usize, &mut exit_code);
        assert_eq!(pid, exit_pid);
        if exit_code != 0 {
            println!("Shell: Process {} exited with code {}", pid, exit_code);
        }
    }
}

fn handle_ls_command(line: &str) {
    let path = if line.len() > 2 {
        line[2..].trim()
    } else {
        "."  // Use current directory instead of root
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

fn handle_cd_command(line: &str) {
    let parts: Vec<&str> = line.split_whitespace().collect();
    let path = if parts.len() < 2 {
        "/"  // Default to root directory if no path specified
    } else {
        parts[1]
    };
    
    let result = user_lib::chdir(path);
    match result {
        0 => {}, // Success, no output needed
        -2 => println!("cd: {}: No such file or directory", path),
        -13 => println!("cd: {}: Permission denied", path),
        -20 => println!("cd: {}: Not a directory", path),
        _ => println!("cd: {}: Unknown error ({})", path, result),
    }
}

fn handle_pwd_command(_line: &str) {
    let mut buf = [0u8; 256];
    let result = user_lib::getcwd(&mut buf);
    if result > 0 {
        // Find the null terminator or use the returned length
        let len = result as usize - 1; // Subtract 1 for null terminator
        if let Ok(cwd) = core::str::from_utf8(&buf[..len]) {
            println!("{}", cwd);
        } else {
            println!("pwd: Invalid UTF-8 in current directory path");
        }
    } else {
        println!("pwd: Cannot get current directory");
    }
}
