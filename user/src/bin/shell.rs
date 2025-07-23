#![no_std]
#![no_main]

#[macro_use]
extern crate alloc;
#[macro_use]
extern crate user_lib;

use alloc::collections::VecDeque;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use user_lib::{chdir, close, dup2, exec, execve, fork, open, pipe, read, wait_pid, yield_};

const LF: u8 = b'\n';
const CR: u8 = b'\r';
const DL: u8 = b'\x7f'; // DEL
const BS: u8 = b'\x08'; // BACKSPACE
const TAB: u8 = b'\t'; // TAB
const ESC: u8 = b'\x1b'; // ESCAPE

// ANSI escape sequences for arrow keys
const ARROW_UP: [u8; 3] = [ESC, b'[', b'A'];
const ARROW_DOWN: [u8; 3] = [ESC, b'[', b'B'];
const ARROW_RIGHT: [u8; 3] = [ESC, b'[', b'C'];
const ARROW_LEFT: [u8; 3] = [ESC, b'[', b'D'];

struct CommandHistory {
    commands: VecDeque<String>,
    current_index: isize,
    max_size: usize,
}

impl CommandHistory {
    fn new(max_size: usize) -> Self {
        CommandHistory {
            commands: VecDeque::new(),
            current_index: -1,
            max_size,
        }
    }

    fn add_command(&mut self, command: String) {
        if !command.is_empty()
            && (self.commands.is_empty() || self.commands.back() != Some(&command))
        {
            if self.commands.len() >= self.max_size {
                self.commands.pop_front();
            }
            self.commands.push_back(command);
        }
        self.current_index = -1; // Reset to current (no history browsing)
    }

    fn get_previous(&mut self) -> Option<&String> {
        if self.commands.is_empty() {
            return None;
        }

        if self.current_index == -1 {
            self.current_index = self.commands.len() as isize - 1;
        } else if self.current_index > 0 {
            self.current_index -= 1;
        }

        self.commands.get(self.current_index as usize)
    }

    fn get_next(&mut self) -> Option<&String> {
        if self.current_index == -1 {
            return None;
        }

        self.current_index += 1;
        if self.current_index >= self.commands.len() as isize {
            self.current_index = -1;
            return None;
        }

        self.commands.get(self.current_index as usize)
    }
}

fn get_char() -> u8 {
    let mut byte = [0u8; 1];
    if read(0, &mut byte) <= 0 {
        return 0;
    }
    byte[0]
}

// 检测ANSI escape sequence
fn detect_escape_sequence() -> Option<[u8; 3]> {
    let first = get_char();
    if first == 0 {
        return None;
    }

    let second = get_char();
    if second == 0 {
        return None;
    }

    let third = get_char();
    if third == 0 {
        return None;
    }

    Some([first, second, third])
}

// 清除当前行并重新显示
fn clear_line_and_redraw(prompt: &str, line: &str) {
    // 移动光标到行首
    print!("\r");
    // 清除整行
    print!("\x1b[K");
    // 重新显示提示符和内容
    print!("{}{}", prompt, line);
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
    let mut history = CommandHistory::new(100); // 保存最多100条历史命令

    // print welcome message
    println!("欢迎使用LiteOS Enhanced Shell!");
    println!("================================");
    println!("输入 'help' 查看可用命令");
    println!("");

    print!("$ ");
    loop {
        let c = get_char();
        match c {
            0 => {
                yield_();
                continue;
            }
            ESC => {
                // 处理escape sequences
                if let Some(seq) = detect_escape_sequence() {
                    match seq {
                        [b'[', b'A', _] => {
                            // 上箭头
                            if let Some(prev_cmd) = history.get_previous() {
                                line = prev_cmd.clone();
                                clear_line_and_redraw("$ ", &line);
                            }
                        }
                        [b'[', b'B', _] => {
                            // 下箭头
                            if let Some(next_cmd) = history.get_next() {
                                line = next_cmd.clone();
                                clear_line_and_redraw("$ ", &line);
                            } else {
                                line.clear();
                                clear_line_and_redraw("$ ", &line);
                            }
                        }
                        [b'[', b'C', _] => { // 右箭头 (暂时忽略)
                            // TODO: 实现光标移动
                        }
                        [b'[', b'D', _] => { // 左箭头 (暂时忽略)
                            // TODO: 实现光标移动
                        }
                        _ => {
                            // 忽略其他escape sequences
                        }
                    }
                }
            }
            CR | LF => {
                println!("");
                if !line.is_empty() {
                    // 将命令添加到历史中
                    history.add_command(line.clone());

                    // 只处理必需的内置命令
                    if line.starts_with("cd") {
                        handle_cd_command(&line);
                    } else if line.starts_with("help") {
                        handle_help_command(&line);
                    } else {
                        // 检查是否包含管道
                        if has_pipe(&line) {
                            // 执行管道命令
                            let commands = parse_pipeline(&line);
                            execute_pipeline(commands);
                        } else {
                            // 执行外部程序，支持重定向和PATH查找
                            execute_command_with_redirection(&line);
                        }
                    }
                    line.clear();
                }
                print!("$ ");
            }
            TAB => {
                // 处理Tab字符 - 扩展为空格直到下一个tab stop
                let current_pos = 2 + string_display_width(&line); // 2 for '$ ' prompt
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
                    let current_pos = 2 + string_display_width(&line); // position after removal, 2 for '$ ' prompt
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

// 检查命令行是否包含管道
fn has_pipe(line: &str) -> bool {
    line.contains('|')
}

// 解析管道命令
fn parse_pipeline(line: &str) -> Vec<String> {
    line.split('|')
        .map(|cmd| cmd.trim().to_string())
        .filter(|cmd| !cmd.is_empty())
        .collect()
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

// 在PATH中查找可执行文件
fn find_in_path(command: &str) -> Option<String> {
    // 如果命令包含路径分隔符，直接返回
    if command.contains('/') {
        if file_exists(command) {
            return Some(String::from(command));
        } else {
            return None;
        }
    }

    // 定义PATH目录列表（简化版本）
    let path_dirs = ["/bin", "/usr/bin", "."];

    for dir in &path_dirs {
        let mut full_path = String::from(*dir);
        full_path.push('/');
        full_path.push_str(command);

        if file_exists(&full_path) {
            return Some(full_path);
        }
    }

    None
}

// 执行管道命令
fn execute_pipeline(commands: Vec<String>) {
    if commands.is_empty() {
        return;
    }

    if commands.len() == 1 {
        // 单个命令，直接执行
        execute_command_with_redirection(&commands[0]);
        return;
    }

    let mut pipes: Vec<[i32; 2]> = Vec::new();
    let mut pids: Vec<isize> = Vec::new();

    // 创建所需的管道
    for _ in 0..(commands.len() - 1) {
        let mut pipefd = [0i32; 2];
        if pipe(&mut pipefd) == -1 {
            println!("shell: failed to create pipe");
            return;
        }
        pipes.push(pipefd);
    }

    // 执行每个命令
    for (i, command) in commands.iter().enumerate() {
        let parts: Vec<&str> = command.split_whitespace().collect();
        if parts.is_empty() {
            continue;
        }

        let first_part = parts[0];

        // 使用PATH查找命令
        let executable_path = if let Some(path) = find_in_path(first_part) {
            path
        } else {
            println!("shell: {}: command not found", first_part);
            continue;
        };

        // 构造参数数组 for execve
        let mut args: Vec<&str> = Vec::new();
        args.push(first_part); // 程序名作为 argv[0] (不是完整路径)
        for j in 1..parts.len() {
            args.push(parts[j]); // 添加其余参数
        }

        // 环境变量（暂时为空）
        let empty_env: Vec<&str> = vec![];

        let pid = fork();
        if pid == 0 {
            // 子进程

            // 设置输入端
            if i > 0 {
                // 不是第一个命令，从前一个管道读取输入
                if dup2(pipes[i - 1][0] as usize, 0) < 0 {
                    println!("shell: failed to redirect input");
                    return;
                }
            }

            // 设置输出端
            if i < commands.len() - 1 {
                // 不是最后一个命令，输出到下一个管道
                if dup2(pipes[i][1] as usize, 1) < 0 {
                    println!("shell: failed to redirect output");
                    return;
                }
            }

            // 关闭所有管道文件描述符
            for pipefd in &pipes {
                close(pipefd[0] as usize);
                close(pipefd[1] as usize);
            }

            // 执行命令
            if execve(&executable_path, &args, &empty_env) == -1 {
                println!("command not found: {}", first_part);
            }
        } else if pid > 0 {
            // 父进程
            pids.push(pid);
        } else {
            println!("shell: failed to fork");
        }
    }

    // 父进程关闭所有管道文件描述符
    for pipefd in &pipes {
        close(pipefd[0] as usize);
        close(pipefd[1] as usize);
    }

    // 等待所有子进程完成
    for pid in pids {
        let mut exit_code: i32 = 0;
        let exit_pid = wait_pid(pid as usize, &mut exit_code);
        if exit_pid != pid {
            println!("shell: warning: unexpected process exit");
        }
    }
}

// 执行带重定向的命令
fn execute_command_with_redirection(line: &str) {
    let (command, output_file, input_file) = parse_command_with_redirection(line);

    if command.is_empty() {
        return;
    }

    // 解析命令和参数
    let parts: Vec<&str> = command.split_whitespace().collect();
    if parts.is_empty() {
        return;
    }

    let first_part = parts[0];

    // 检查是否为直接执行 WASM 文件
    if is_wasm_file(first_part) {
        if file_exists(first_part) {
            // 构造新的命令：wasm_runtime <wasm_file> [args...]
            let mut wasm_command = String::from("/bin/wasm_runtime ");
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
            let mut wasm_command = String::from("/bin/wasm_runtime ");
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

    // 使用PATH查找命令
    let executable_path = if let Some(path) = find_in_path(first_part) {
        path
    } else {
        println!("shell: {}: command not found", first_part);
        return;
    };

    // 构造参数数组 for execve
    let mut args: Vec<&str> = Vec::new();
    args.push(first_part); // 程序名作为 argv[0] (不是完整路径)
    for i in 1..parts.len() {
        args.push(parts[i]); // 添加其余参数
    }

    // 环境变量（暂时为空）
    let empty_env: Vec<&str> = vec![];

    let pid = fork();
    if pid == 0 {
        // 子进程：设置重定向并执行命令

        // 设置输入重定向
        if let Some(input_filename) = input_file {
            let mut input_filename_with_null = input_filename;
            input_filename_with_null.push('\0');
            let input_fd = open(input_filename_with_null.as_str(), 0);
            if input_fd < 0 {
                println!(
                    "shell: {}: No such file or directory",
                    input_filename_with_null.trim_end_matches('\0')
                );
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
                println!(
                    "shell: failed to create output file: {}",
                    output_filename_with_null.trim_end_matches('\0')
                );
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

        // 使用 execve 执行命令
        if execve(&executable_path, &args, &empty_env) == -1 {
            println!("command not found: {}", first_part);
        }
    } else {
        // 父进程：等待子进程完成
        let mut exit_code: i32 = 0;
        let exit_pid = wait_pid(pid as usize, &mut exit_code);
        assert_eq!(pid, exit_pid);
    }
}

// 执行 WASM 命令（通过 wasm_runtime）
fn execute_wasm_command(
    wasm_command: &str,
    output_file: Option<String>,
    input_file: Option<String>,
) {
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
                println!(
                    "shell: {}: No such file or directory",
                    input_filename_with_null.trim_end_matches('\0')
                );
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
                println!(
                    "shell: failed to create output file: {}",
                    output_filename_with_null.trim_end_matches('\0')
                );
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

fn handle_cd_command(line: &str) {
    let parts: Vec<&str> = line.split_whitespace().collect();
    let path = if parts.len() < 2 {
        "/" // Default to root directory if no path specified
    } else {
        parts[1]
    };

    let result = chdir(path);
    match result {
        0 => {} // Success, no output needed
        -2 => println!("cd: {}: No such file or directory", path),
        -13 => println!("cd: {}: Permission denied", path),
        -20 => println!("cd: {}: Not a directory", path),
        _ => println!("cd: {}: Unknown error ({})", path, result),
    }
}

fn handle_help_command(_line: &str) {
    println!("LiteOS Shell - Enhanced Unix-like Shell");
    println!("======================================");
    println!("");
    println!("Built-in Commands:");
    println!("  cd [dir]           - Change directory");
    println!("  help               - Show this help message");
    println!("  exit [code]        - Exit shell with optional exit code");
    println!("");
    println!("External Programs (via PATH):");
    println!("  ls [dir]           - List directory contents");
    println!("  cat <file>         - Display file contents");
    println!("  mkdir <dir>        - Create directory");
    println!("  rm <file>          - Remove file");
    println!("  pwd                - Print working directory");
    println!("  <program>          - Execute any program in PATH");
    println!("");
    println!("WASM Programs:");
    println!("  <file>.wasm        - Execute WASM program (auto-detected)");
    println!("  ./<file>.wasm      - Execute WASM program with relative path");
    println!("  wasm_runtime <file>.wasm - Execute WASM program explicitly");
    println!("");
    println!("I/O Redirection:");
    println!("  <command> > file   - Redirect output to file");
    println!("  <command> < file   - Redirect input from file");
    println!("");
    println!("Pipes:");
    println!("  <cmd1> | <cmd2>    - Pipe output of cmd1 to input of cmd2");
    println!("  <cmd1> | <cmd2> | <cmd3> - Chain multiple commands");
    println!("");
    println!("PATH Search Order:");
    println!("  1. /bin/           - System binaries");
    println!("  2. /usr/bin/       - User binaries");
    println!("  3. ./              - Current directory");
    println!("");
    println!("Examples:");
    println!("  ls /               - List root directory");
    println!("  cat README.txt     - Display file contents");
    println!("  mkdir testdir      - Create directory");
    println!("  hello_wasm.wasm    - Run WASM program");
    println!("  ls > files.txt     - Save directory listing");
    println!("  ls | cat           - List files and display through cat");
    println!("  echo hello | cat   - Echo text and pipe to cat");
    println!("");
}
