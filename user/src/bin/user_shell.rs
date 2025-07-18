#![no_std]
#![no_main]

#[macro_use]
extern crate alloc;
#[macro_use]
extern crate user_lib;

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use user_lib::{exec, execve, fork, read, wait_pid, yield_, open, close, dup2};

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
                    } else if line.starts_with("help") {
                        handle_help_command(&line);
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

// 检查是否为 WASM 文件
fn is_wasm_file(filename: &str) -> bool {
    filename.ends_with(".wasm")
}

// 检查文件是否存在（简化版本）
fn file_exists(filename: &str) -> bool {
    let fd = open(filename, 0); // O_RDONLY
    if fd >= 0 {
        close(fd as usize);
        true
    } else {
        false
    }
}

// 执行带重定向的命令
fn execute_command_with_redirection(line: &str) {
    let (command, output_file, input_file) = parse_command_with_redirection(line);
    
    if command.is_empty() {
        return;
    }
    
    // 检查是否为直接执行 WASM 文件
    let parts: Vec<&str> = command.split_whitespace().collect();
    if !parts.is_empty() {
        let first_part = parts[0];
        
        // 如果命令是 .wasm 文件，自动使用 wasm_runtime 执行
        if is_wasm_file(first_part) {
            if file_exists(first_part) {
                // 构造新的命令：wasm_runtime <wasm_file> [args...]
                let mut wasm_command = String::from("wasm_runtime ");
                wasm_command.push_str(&command);
                execute_wasm_command(&wasm_command, output_file, input_file);
                return;
            } else {
                println!("shell: {}: No such file or directory", first_part);
                return;
            }
        }
        
        // 如果命令以 ./ 开头且是 .wasm 文件，也自动使用 wasm_runtime
        if first_part.starts_with("./") && is_wasm_file(first_part) {
            let wasm_file = &first_part[2..]; // 去掉 "./"
            if file_exists(wasm_file) {
                let mut wasm_command = String::from("wasm_runtime ");
                wasm_command.push_str(wasm_file);
                
                // 添加其他参数
                for i in 1..parts.len() {
                    wasm_command.push(' ');
                    wasm_command.push_str(parts[i]);
                }
                
                execute_wasm_command(&wasm_command, output_file, input_file);
                return;
            } else {
                println!("shell: {}: No such file or directory", wasm_file);
                return;
            }
        }
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

// 执行 WASM 命令（通过 wasm_runtime）
fn execute_wasm_command(wasm_command: &str, output_file: Option<String>, input_file: Option<String>) {
    // 解析命令和参数
    let parts: Vec<&str> = wasm_command.split_whitespace().collect();
    if parts.is_empty() {
        return;
    }
    
    let program = parts[0]; // "wasm_runtime"
    let args: Vec<&str> = parts.iter().map(|&s| s).collect();
    
    let pid = fork();
    if pid == 0 {
        // 子进程：设置重定向并执行 WASM 运行时
        
        // 设置输入重定向
        if let Some(input_filename) = input_file {
            let mut input_filename_with_null = input_filename;
            input_filename_with_null.push('\0');
            let input_fd = open(input_filename_with_null.as_str(), 0);
            if input_fd < 0 {
                println!("shell: {}: No such file or directory", input_filename_with_null.trim_end_matches('\0'));
                return;
            }
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
            if dup2(output_fd as usize, 1) < 0 {
                println!("shell: failed to redirect output");
                close(output_fd as usize);
                return;
            }
            close(output_fd as usize);
        }
        
        // 执行 WASM 运行时 - 使用 execve 来传递参数
        let empty_env: Vec<&str> = vec![];
        if execve(program, &args, &empty_env) == -1 {
            println!("wasm_runtime not found - please ensure wasm_runtime is in the filesystem");
        }
    } else {
        // 父进程：等待子进程完成
        let mut exit_code: i32 = 0;
        let exit_pid = wait_pid(pid as usize, &mut exit_code);
        assert_eq!(pid, exit_pid);
        if exit_code != 0 {
            println!("Shell: WASM process {} exited with code {}", pid, exit_code);
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

fn handle_help_command(_line: &str) {
    println!("LiteOS Shell - Built-in Commands:");
    println!("================================");
    println!("");
    println!("File Operations:");
    println!("  ls [dir]           - List directory contents");
    println!("  cat <file>         - Display file contents");
    println!("  mkdir <dir>        - Create directory");
    println!("  rm <file>          - Remove file");
    println!("  pwd                - Print working directory");
    println!("  cd [dir]           - Change directory");
    println!("");
    println!("Program Execution:");
    println!("  <program>          - Execute ELF binary");
    println!("  <file>.wasm        - Execute WASM program (auto-detected)");
    println!("  ./<file>.wasm      - Execute WASM program with relative path");
    println!("  wasm_runtime <file>.wasm - Execute WASM program explicitly");
    println!("");
    println!("I/O Redirection:");
    println!("  <command> > file   - Redirect output to file");
    println!("  <command> < file   - Redirect input from file");
    println!("");
    println!("Examples:");
    println!("  hello_wasm.wasm              - Run Rust WASM test program");
    println!("  ./math_test.wasm             - Run math operations test");
    println!("  wasi_test.wasm > output.txt  - Run WASI test, save output");
    println!("  file_test.wasm               - Run file operations test");
    println!("");
    println!("Available WASM Programs:");
    println!("  hello_wasm.wasm    - Basic Rust WASM hello world");
    println!("  wasi_test.wasm     - Comprehensive WASI functionality test");
    println!("  math_test.wasm     - Mathematical operations test");
    println!("  file_test.wasm     - File I/O operations test");
    println!("  simple.wasm        - Minimal WASM (returns 42)");
    println!("  hello.wasm         - Simple hello message");
    println!("");
    println!("Other Commands:");
    println!("  help               - Show this help message");
    println!("");
    println!("Note: WASM files are automatically detected and executed");
    println!("      through the wasm_runtime when run directly.");
}
