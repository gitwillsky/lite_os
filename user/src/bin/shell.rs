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
use user_lib::{read, yield_, getcwd};

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

    let second = get_char();
    if second == 0 {
        return None;
    }

    let third = get_char();
    if third == 0 {
        return None;
    }

    // 检查是否是4字节序列（如Delete键）
    if first == b'[' && second == b'3' && third == b'~' {
        return Some(vec![first, second, third]);
    }

    // 检查是否需要读取第四个字节
    if first == b'[' && second.is_ascii_digit() {
        return Some(vec![first, second, third]);
    }

    Some(vec![first, second, third])
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

    // 打印欢迎信息
    println!("欢迎使用LiteOS Enhanced Shell!");
    println!("================================");
    println!("输入 'help' 查看可用命令");
    println!("");

    let prompt = generate_prompt();
    print!("{}", prompt);
    loop {
        let c = get_char();
        match c {
            0 => {
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
                    if let Err(e) = job_manager.terminate_foreground_job() {
                        println!("错误: {}", e);
                    }
                } else {
                    println!("");
                    editor.clear();
                }
                let current_prompt = generate_prompt();
                print!("{}", current_prompt);
            }
            CTRL_Z => {
                // Ctrl+Z - 挂起前台作业
                if job_manager.get_foreground_job().is_some() {
                    if let Err(e) = job_manager.suspend_foreground_job() {
                        println!("错误: {}", e);
                    }
                    let current_prompt = generate_prompt();
                    print!("{}", current_prompt);
                } else {
                    // 如果没有前台作业，忽略Ctrl+Z
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
                    if seq.len() >= 3 {
                        match (seq[0], seq[1], seq[2]) {
                            (b'[', b'A', _) => {
                                // 上箭头 - 历史记录上一条
                                if let Some(prev_cmd) = history.get_previous() {
                                    editor.set_content(prev_cmd.clone());
                                    let current_prompt = generate_prompt();
                                    editor.redraw_line(&current_prompt);
                                }
                            }
                            (b'[', b'B', _) => {
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
                            (b'[', b'C', _) => {
                                // 右箭头 - 光标右移
                                if editor.move_cursor_right() {
                                    let current_prompt = generate_prompt();
                                    editor.redraw_line(&current_prompt);
                                }
                            }
                            (b'[', b'D', _) => {
                                // 左箭头 - 光标左移
                                if editor.move_cursor_left() {
                                    let current_prompt = generate_prompt();
                                    editor.redraw_line(&current_prompt);
                                }
                            }
                            (b'[', b'H', _) => {
                                // Home键 - 移动到行首
                                if editor.move_cursor_home() {
                                    let current_prompt = generate_prompt();
                                    editor.redraw_line(&current_prompt);
                                }
                            }
                            (b'[', b'F', _) => {
                                // End键 - 移动到行尾
                                if editor.move_cursor_end() {
                                    let current_prompt = generate_prompt();
                                    editor.redraw_line(&current_prompt);
                                }
                            }
                            (b'[', b'3', b'~') => {
                                // Delete键 - 删除当前字符
                                if editor.delete_char_forward() {
                                    let current_prompt = generate_prompt();
                                    editor.redraw_line(&current_prompt);
                                }
                            }
                            _ => {
                                // 忽略其他escape sequences
                            }
                        }
                    }
                }
            }
            CR | LF => {
                println!("");
                let line = editor.content();
                if !line.is_empty() {
                    // 将命令添加到历史中
                    history.add_command(line.to_string());

                    // 处理内置命令
                    if line.starts_with("cd") {
                        handle_cd_command(line);
                    } else if line.starts_with("help") {
                        handle_help_command(line);
                    } else if line == "jobs" {
                        job_manager.list_jobs();
                    } else if line.starts_with("fg") {
                        handle_fg_command(line, &mut job_manager);
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
                        } else {
                            // 执行外部程序，支持重定向和PATH查找
                            execute_command_with_jobs(&command_line, is_background, &mut job_manager);
                        }
                    }
                    editor.clear();
                }
                // 检查作业状态
                job_manager.check_job_status();
                job_manager.cleanup_finished_jobs();
                let current_prompt = generate_prompt();
                print!("{}", current_prompt);
            }
            TAB => {
                // Tab补全功能
                let current_prompt = generate_prompt();
                TabCompletion::complete(&mut editor, &current_prompt);
            }
            BS | DL => {
                // 退格键 - 删除光标前的字符
                if editor.delete_char_backward() {
                    let current_prompt = generate_prompt();
                    editor.redraw_line(&current_prompt);
                }
            }
            _ => {
                // 普通字符输入
                if c >= 32 && c < 127 { // 只处理可打印的ASCII字符
                    editor.insert_char(c as char);
                    let current_prompt = generate_prompt();
                    editor.redraw_line(&current_prompt);
                }
            }
        }
    }
    0
}