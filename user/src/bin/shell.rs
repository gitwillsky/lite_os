#![no_std]
#![no_main]

#[macro_use]
extern crate alloc;
#[macro_use]
extern crate user_lib;

mod shell_modules;

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use shell_modules::*;
use user_lib::{check_keyboard_input, getcwd, read, yield_};

// 控制字符常量
const LF: u8 = b'\n';
const CR: u8 = b'\r';
const DL: u8 = b'\x7f'; // DEL
const BS: u8 = b'\x08'; // BACKSPACE
const TAB: u8 = b'\t'; // TAB
const ESC: u8 = b'\x1b'; // ESCAPE
const CTRL_A: u8 = b'\x01'; // Ctrl+A
const CTRL_E: u8 = b'\x05'; // Ctrl+E
const CTRL_D: u8 = b'\x04'; // Ctrl+D
const CTRL_C: u8 = b'\x03'; // Ctrl+C
const CTRL_Z: u8 = b'\x1a'; // Ctrl+Z

/// 获取单个字符输入
fn get_char() -> u8 {
    let mut byte = [0u8; 1];
    if read(0, &mut byte) <= 0 {
        return 0;
    }
    byte[0]
}

/// 检测ANSI escape sequence
fn detect_escape_sequence() -> Option<Vec<u8>> {
    let first = get_char();
    if first == 0 {
        return None;
    }

    // 检查是否是CSI序列 (ESC[)
    if first == b'[' {
        let second = get_char();
        if second == 0 {
            return None;
        }

        // 对于简单的箭头键 (A, B, C, D)，只需要两个字节
        if matches!(second, b'A' | b'B' | b'C' | b'D' | b'H' | b'F') {
            return Some(vec![first, second]);
        }

        // 对于其他序列，可能需要更多字节
        if second.is_ascii_digit() {
            let third = get_char();
            if third == 0 {
                return None;
            }

            // 检查是否是Delete键等（如3~）
            if third == b'~' {
                return Some(vec![first, second, third]);
            }

            // 其他数字序列，返回目前读到的
            return Some(vec![first, second, third]);
        }

        // 其他单字符序列
        return Some(vec![first, second]);
    }

    // 非CSI序列，返回单个字符
    Some(vec![first])
}

/// 生成包含当前目录的提示符
fn generate_prompt() -> String {
    let mut buf = [0u8; 1024]; // 增大缓冲区以支持长路径
    let result = getcwd(&mut buf);

    if result >= 0 {
        // 找到字符串结尾
        let mut end = 0;
        for i in 0..buf.len() {
            if buf[i] == 0 {
                end = i;
                break;
            }
        }

        if end > 0 {
            if let Ok(path) = core::str::from_utf8(&buf[0..end]) {
                // 显示完整路径
                return format!("{}$ ", path);
            }
        }
    }

    // 如果获取当前目录失败，使用默认提示符
    String::from("$ ")
}

#[unsafe(no_mangle)]
fn main() -> i32 {
    let mut editor = LineEditor::new();
    let mut history = CommandHistory::new(100); // 保存最多100条历史命令
    let mut job_manager = JobManager::new();

    let prompt = generate_prompt();
    print!("{}", prompt);
    loop {
        let c = check_keyboard_input(true).unwrap_or(0);
        match c {
            0 => {
                // 检查作业状态，即使没有输入
                let foreground_completed = job_manager.check_job_status();
                job_manager.cleanup_finished_jobs();
                job_manager.reap_zombies(); // 主动回收任何未跟踪的zombie进程

                // 如果前台作业完成，打印提示符
                if foreground_completed {
                    let current_prompt = generate_prompt();
                    print!("{}", current_prompt);
                }

                yield_();
                continue;
            }
            CTRL_D => {
                // Ctrl+D - 退出shell
                if editor.content().is_empty() {
                    break;
                } else {
                    // 如果有内容，则删除当前字符
                    if editor.delete_char_forward() {
                        let current_prompt = generate_prompt();
                        editor.redraw_line(&current_prompt);
                    }
                }
            }
            CTRL_C => {
                // Ctrl+C - 终止前台作业或取消当前命令
                if job_manager.get_foreground_job().is_some() {
                    let _ = job_manager.terminate_foreground_job();
                } else {
                    println!("");
                    editor.clear();
                }
                let current_prompt = generate_prompt();
                print!("{}", current_prompt);
            }
            CTRL_Z => {
                // Ctrl+Z - 挂起前台作业
                if let Some(_fg_job) = job_manager.get_foreground_job() {
                    let _ = job_manager.suspend_foreground_job();
                    let current_prompt = generate_prompt();
                    print!("{}", current_prompt);
                } else {
                    // 如果没有前台作业，忽略Ctrl+Z但显示信息
                    println!(""); // 换行
                    println!("shell: no job to suspend");
                    let current_prompt = generate_prompt();
                    print!("{}", current_prompt);
                }
            }
            CTRL_A => {
                // Ctrl+A - 移动到行首
                if editor.move_cursor_home() {
                    let current_prompt = generate_prompt();
                    editor.redraw_line(&current_prompt);
                }
            }
            CTRL_E => {
                // Ctrl+E - 移动到行尾
                if editor.move_cursor_end() {
                    let current_prompt = generate_prompt();
                    editor.redraw_line(&current_prompt);
                }
            }
            ESC => {
                // 处理escape sequences
                if let Some(seq) = detect_escape_sequence() {
                    match seq.len() {
                        2 => {
                            // 2字节序列（箭头键等）
                            match (seq[0], seq[1]) {
                                (b'[', b'A') => {
                                    // 上箭头 - 历史记录上一条
                                    if let Some(prev_cmd) = history.get_previous() {
                                        editor.set_content(prev_cmd.clone());
                                        let current_prompt = generate_prompt();
                                        editor.redraw_line(&current_prompt);
                                    }
                                }
                                (b'[', b'B') => {
                                    // 下箭头 - 历史记录下一条
                                    if let Some(next_cmd) = history.get_next() {
                                        editor.set_content(next_cmd.clone());
                                        let current_prompt = generate_prompt();
                                        editor.redraw_line(&current_prompt);
                                    } else {
                                        editor.clear();
                                        let current_prompt = generate_prompt();
                                        editor.redraw_line(&current_prompt);
                                    }
                                }
                                (b'[', b'C') => {
                                    // 右箭头 - 光标右移
                                    if editor.move_cursor_right() {
                                        let current_prompt = generate_prompt();
                                        editor.redraw_line(&current_prompt);
                                    }
                                }
                                (b'[', b'D') => {
                                    // 左箭头 - 光标左移
                                    if editor.move_cursor_left() {
                                        let current_prompt = generate_prompt();
                                        editor.redraw_line(&current_prompt);
                                    }
                                }
                                (b'[', b'H') => {
                                    // Home键 - 移动到行首
                                    if editor.move_cursor_home() {
                                        let current_prompt = generate_prompt();
                                        editor.redraw_line(&current_prompt);
                                    }
                                }
                                (b'[', b'F') => {
                                    // End键 - 移动到行尾
                                    if editor.move_cursor_end() {
                                        let current_prompt = generate_prompt();
                                        editor.redraw_line(&current_prompt);
                                    }
                                }
                                _ => {
                                    // 忽略其他2字节序列
                                }
                            }
                        }
                        3 => {
                            // 3字节序列（Delete键等）
                            match (seq[0], seq[1], seq[2]) {
                                (b'[', b'3', b'~') => {
                                    // Delete键 - 删除当前字符
                                    if editor.delete_char_forward() {
                                        let current_prompt = generate_prompt();
                                        editor.redraw_line(&current_prompt);
                                    }
                                }
                                _ => {
                                    // 忽略其他3字节序列
                                }
                            }
                        }
                        _ => {
                            // 忽略其他长度的序列
                        }
                    }
                }
            }
            CR | LF => {
                println!("");
                let line = editor.content();
                if !line.is_empty() {
                    let mut suppress_prompt = false; // 是否抑制提示符（前台作业时不立即打印）
                    // 将命令添加到历史中
                    history.add_command(line.to_string());

                    // 处理内置命令
                    if line.starts_with("cd") {
                        handle_cd_command(line);
                    } else if line.starts_with("help") {
                        handle_help_command(line);
                    } else if line == "clear" {
                        print!("\x1b[2J\x1b[H");
                    } else if line == "jobs" {
                        job_manager.list_jobs();
                    } else if line.starts_with("fg") {
                        handle_fg_command(line, &mut job_manager);
                        // fg 命令后不立即显示提示符，让前台作业运行
                        editor.clear();
                        continue;
                    } else if line.starts_with("bg") {
                        handle_bg_command(line, &mut job_manager);
                    } else {
                        // 检查是否为后台命令
                        let (command_line, is_background) = if line.trim_end().ends_with('&') {
                            let cmd = line.trim_end();
                            let cmd = cmd[..cmd.len() - 1].trim_end();
                            (String::from(cmd), true)
                        } else {
                            (String::from(line), false)
                        };

                        // 检查是否包含管道
                        if has_pipe(&command_line) {
                            // 执行管道命令
                            let commands = parse_pipeline(&command_line);
                            execute_pipeline_with_jobs(commands, is_background, &mut job_manager);
                            // 管道：前台在此已等待完成，应立即打印提示符；后台也应立即打印
                            suppress_prompt = false;
                        } else {
                            // 执行外部程序，支持重定向和PATH查找
                            execute_command_with_jobs(
                                &command_line,
                                is_background,
                                &mut job_manager,
                            );
                            // 外部命令：前台不应立即打印提示符，等待异步完成；后台则立即打印
                            suppress_prompt = !is_background;
                        }
                    }
                    editor.clear();
                    // 清理已完成的作业
                    job_manager.cleanup_finished_jobs();
                    job_manager.reap_zombies(); // 主动回收任何未跟踪的zombie进程

                    // 根据是否需要抑制提示符来决定是否立即打印
                    if !suppress_prompt {
                        let current_prompt = generate_prompt();
                        print!("{}", current_prompt);
                    }
                } else {
                    // 空命令行：直接重新显示提示符
                    let current_prompt = generate_prompt();
                    print!("{}", current_prompt);
                }
            }
            TAB => {
                // Tab补全功能
                let current_prompt = generate_prompt();
                TabCompletion::complete(&mut editor, &current_prompt);
            }
            BS | DL => {
                // 退格键 - 删除光标前的字符
                let current_prompt = generate_prompt();
                editor.delete_char_backward_optimized(&current_prompt);
            }
            _ => {
                // 普通字符输入
                if c >= 32 && c < 127 {
                    // 只处理可打印的ASCII字符
                    let current_prompt = generate_prompt();
                    editor.insert_char_optimized(c as char, &current_prompt);
                }
            }
        }
    }
    0
}
