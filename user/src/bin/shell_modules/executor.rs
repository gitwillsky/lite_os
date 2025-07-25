//! 命令执行模块

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use user_lib::{close, dup2, execve, fork, open, pipe, wait_pid};
use super::jobs::{JobManager, JobStatus};

/// 检查命令行是否包含管道
pub fn has_pipe(line: &str) -> bool {
    line.contains('|')
}

/// 解析管道命令
pub fn parse_pipeline(line: &str) -> Vec<String> {
    line.split('|')
        .map(|cmd| cmd.trim().to_string())
        .filter(|cmd| !cmd.is_empty())
        .collect()
}

/// 解析命令和重定向
pub fn parse_command_with_redirection(line: &str) -> (String, Option<String>, Option<String>) {
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

/// 检查是否为 WASM 文件
pub fn is_wasm_file(filename: &str) -> bool {
    filename.ends_with(".wasm")
}

/// 检查文件是否存在（简化版本）
pub fn file_exists(filename: &str) -> bool {
    let fd = open(filename, 0); // O_RDONLY
    if fd >= 0 {
        close(fd as usize);
        true
    } else {
        false
    }
}

/// 在PATH中查找可执行文件
pub fn find_in_path(command: &str) -> Option<String> {
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

    for &dir in &path_dirs {
        let mut full_path = String::from(dir);
        full_path.push('/');
        full_path.push_str(command);

        if file_exists(&full_path) {
            return Some(full_path);
        }
    }

    None
}

/// 执行带作业控制的命令
pub fn execute_command_with_jobs(
    line: &str,
    background: bool,
    job_manager: &mut JobManager,
) {
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
            execute_wasm_command_with_jobs(&wasm_command, output_file, input_file, background, job_manager);
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

            execute_wasm_command_with_jobs(&wasm_command, output_file, input_file, background, job_manager);
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
    } else if pid > 0 {
        // 父进程：添加作业
        let job_id = job_manager.add_job(pid, String::from(line), background);

        if background {
            println!("[{}] {}", job_id, pid);
        }
    } else {
        println!("shell: failed to fork");
    }
}

/// 执行带作业控制的WASM命令
pub fn execute_wasm_command_with_jobs(
    wasm_command: &str,
    output_file: Option<String>,
    input_file: Option<String>,
    background: bool,
    job_manager: &mut JobManager,
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
    } else if pid > 0 {
        // 父进程：添加作业
        let job_id = job_manager.add_job(pid, String::from(wasm_command), background);

        if background {
            println!("[{}] {}", job_id, pid);
        }
    } else {
        println!("shell: failed to fork");
    }
}

/// 执行带作业控制的管道命令
pub fn execute_pipeline_with_jobs(
    commands: Vec<String>,
    background: bool,
    job_manager: &mut JobManager,
) {
    if commands.is_empty() {
        return;
    }

    if commands.len() == 1 {
        // 单个命令，直接执行
        execute_command_with_jobs(&commands[0], background, job_manager);
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

    // 添加管道作业（使用第一个进程的PID作为作业代表）
    if !pids.is_empty() {
        let pipeline_command = commands.join(" | ");
        let job_id = job_manager.add_job(pids[0], pipeline_command, background);

        if background {
            println!("[{}] {}", job_id, pids[0]);
        }
    }

    // 等待所有子进程完成
    if !background {
        for &pid in &pids {
            let mut exit_code: i32 = 0;
            let exit_pid = wait_pid(pid as usize, &mut exit_code);
            if exit_pid != pid {
                println!("shell: warning: unexpected process exit");
            }
        }
        // 更新作业状态
        if !pids.is_empty() {
            if let Some(job) = job_manager.get_job_by_pid(pids[0]) {
                job.status = JobStatus::Done;
            }
        }
    }
}