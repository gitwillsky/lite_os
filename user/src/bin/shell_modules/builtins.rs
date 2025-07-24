//! 内置命令处理模块

use alloc::vec::Vec;
use user_lib::chdir;
use super::jobs::JobManager;

/// 处理cd命令
pub fn handle_cd_command(line: &str) {
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

/// 处理help命令
pub fn handle_help_command(_line: &str) {
    println!("LiteOS Shell - Enhanced Unix-like Shell");
    println!("======================================");
    println!("");
    println!("Shell Features:");
    println!("  • Current directory display in prompt");
    println!("  • Command history (Up/Down arrows)");
    println!("  • Line editing with cursor movement");
    println!("  • Tab completion for files and commands");
    println!("  • Job control and background execution");
    println!("  • Signal handling (Ctrl+C, Ctrl+Z, Ctrl+D)");
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
    println!("Job Control:");
    println!("  <command> &        - Run command in background");
    println!("  jobs               - List active jobs");
    println!("  fg [%job_id]       - Bring job to foreground");
    println!("  bg [%job_id]       - Send job to background");
    println!("  Ctrl+Z             - Suspend current foreground job");
    println!("  Ctrl+C             - Terminate current foreground job");
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

/// 处理fg命令
pub fn handle_fg_command(line: &str, job_manager: &mut JobManager) {
    let parts: Vec<&str> = line.split_whitespace().collect();
    
    if parts.len() > 1 {
        // 指定作业 ID
        let job_spec = parts[1];
        if let Some(job_id_str) = job_spec.strip_prefix('%') {
            if let Ok(job_id) = job_id_str.parse::<usize>() {
                if let Err(e) = job_manager.bring_to_foreground(job_id) {
                    println!("fg: {}", e);
                }
            } else {
                println!("fg: {}: 不是有效的作业编号", job_id_str);
            }
        } else {
            println!("fg: 使用格式: fg %job_id");
        }
    } else {
        // 没有指定作业，使用最近的停止作业
        println!("fg: 请指定作业编号，使用 jobs 命令查看可用作业");
    }
}

/// 处理bg命令
pub fn handle_bg_command(line: &str, job_manager: &mut JobManager) {
    let parts: Vec<&str> = line.split_whitespace().collect();
    
    if parts.len() > 1 {
        // 指定作业 ID
        let job_spec = parts[1];
        if let Some(job_id_str) = job_spec.strip_prefix('%') {
            if let Ok(job_id) = job_id_str.parse::<usize>() {
                if let Err(e) = job_manager.send_to_background(job_id) {
                    println!("bg: {}", e);
                }
            } else {
                println!("bg: {}: 不是有效的作业编号", job_id_str);
            }
        } else {
            println!("bg: 使用格式: bg %job_id");
        }
    } else {
        // 没有指定作业，使用最近的停止作业
        println!("bg: 请指定作业编号，使用 jobs 命令查看可用作业");
    }
}