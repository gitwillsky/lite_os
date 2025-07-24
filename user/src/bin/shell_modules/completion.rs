//! Tab补全系统

use super::editor::LineEditor;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use user_lib::{close, listdir, open, stat};

/// Tab补全系统
pub struct TabCompletion;

impl TabCompletion {
    /// 执行Tab补全
    pub fn complete(editor: &mut LineEditor, prompt: &str) {
        let content = editor.content();
        let cursor_pos = editor.cursor_pos();

        // 解析当前光标位置的单词
        let (word_start, word_end, word) = Self::extract_word_at_cursor(content, cursor_pos);

        // 获取补全候选
        let candidates = Self::get_completion_candidates(content, word_start, &word);

        if candidates.len() > 1 && candidates.len() < 5 {
            for candidate in candidates.iter() {
                println!("{}", candidate);
            }
        }

        if candidates.is_empty() {
            // 没有候选，插入Tab字符
            editor.insert_char('\t');
            editor.redraw_line(prompt);
        } else if candidates.len() == 1 {
            // 只有一个候选，直接补全
            let candidate = &candidates[0];
            Self::apply_completion(editor, word_start, word_end, candidate);
            editor.redraw_line(prompt);
        } else {
            // 多个候选，显示列表并补全公共前缀
            Self::show_candidates(&candidates);
            let common_prefix = Self::find_common_prefix(&candidates);
            if common_prefix.len() > word.len() {
                Self::apply_completion(editor, word_start, word_end, &common_prefix);
            }
            editor.redraw_line(prompt);
        }
    }

    /// 提取光标位置的单词
    fn extract_word_at_cursor(content: &str, cursor_pos: usize) -> (usize, usize, String) {
        let chars: Vec<char> = content.chars().collect();

        // 查找单词边界
        let mut word_start = cursor_pos;
        while word_start > 0 {
            let prev_char = chars[word_start - 1];
            if prev_char.is_whitespace()
                || prev_char == '|'
                || prev_char == '&'
                || prev_char == '>'
                || prev_char == '<'
                || prev_char == ';'
            {
                break;
            }
            word_start -= 1;
        }

        let mut word_end = cursor_pos;
        while word_end < chars.len() {
            let curr_char = chars[word_end];
            if curr_char.is_whitespace()
                || curr_char == '|'
                || curr_char == '&'
                || curr_char == '>'
                || curr_char == '<'
                || curr_char == ';'
            {
                break;
            }
            word_end += 1;
        }

        let word: String = chars[word_start..cursor_pos].iter().collect();
        (word_start, word_end, word)
    }

    /// 获取补全候选
    fn get_completion_candidates(content: &str, word_start: usize, word: &str) -> Vec<String> {
        // 判断是否为第一个单词（命令）
        let is_command = Self::is_first_word(content, word_start);

        if is_command {
            // 补全命令名
            Self::complete_command(word)
        } else {
            // 补全文件名/目录名
            Self::complete_path(word)
        }
    }

    /// 判断是否为第一个单词
    fn is_first_word(content: &str, word_start: usize) -> bool {
        let prefix = &content[..word_start];
        let trimmed = prefix.trim_start();

        // 检查是否在管道或重定向之后
        for separator in ['|', '&', ';'].iter() {
            if let Some(pos) = trimmed.rfind(*separator) {
                let after_sep = &trimmed[pos + 1..].trim_start();
                return after_sep.is_empty();
            }
        }

        // 如果没有分隔符，检查是否为空或只有空白
        trimmed.is_empty()
    }

    /// 补全命令名
    fn complete_command(prefix: &str) -> Vec<String> {
        let mut candidates = Vec::new();

        // 内置命令
        let builtins = [
            "cd", "help", "exit", "jobs", "fg", "bg", "set", "unset", "export",
        ];
        for &builtin in &builtins {
            if builtin.starts_with(prefix) {
                candidates.push(String::from(builtin));
            }
        }

        // PATH中的命令
        let path_dirs = ["/bin", "/usr/bin", "."];
        for &dir in &path_dirs {
            if let Ok(entries) = Self::list_directory_entries(dir) {
                for entry in entries {
                    if entry.starts_with(prefix) {
                        let executable = Self::is_executable(&entry, dir);
                        if executable {
                            candidates.push(entry);
                        }
                    }
                }
            }
        }

        candidates.sort();
        candidates.dedup();
        candidates
    }

    /// 补全文件/目录名
    fn complete_path(prefix: &str) -> Vec<String> {
        let mut candidates = Vec::new();

        // 解析路径
        let (dir_path, file_prefix) = if prefix.contains('/') {
            // 包含路径分隔符
            if let Some(last_slash) = prefix.rfind('/') {
                let dir = if last_slash == 0 {
                    "/" // 根目录
                } else {
                    &prefix[..last_slash]
                };
                let file = &prefix[last_slash + 1..];
                (String::from(dir), String::from(file))
            } else {
                (String::from("."), String::from(prefix))
            }
        } else {
            // 没有路径分隔符，在当前目录搜索
            (String::from("."), String::from(prefix))
        };

        // 列出目录内容
        if let Ok(entries) = Self::list_directory_entries(&dir_path) {
            for entry in entries {
                if entry.starts_with(&file_prefix) {
                    let full_path = if dir_path == "." {
                        entry.clone()
                    } else if dir_path == "/" {
                        format!("/{}", entry)
                    } else {
                        format!("{}/{}", dir_path, entry)
                    };

                    // 构建正确的补全候选项
                    let candidate = if prefix.contains('/') {
                        // 如果原前缀包含路径，保持目录部分
                        let base_dir = &prefix[..prefix.rfind('/').unwrap() + 1];
                        if Self::is_directory(&full_path) {
                            format!("{}{}/", base_dir, entry)
                        } else {
                            format!("{}{}", base_dir, entry)
                        }
                    } else {
                        // 如果是当前目录，只用文件名
                        if Self::is_directory(&full_path) {
                            format!("{}/", entry)
                        } else {
                            entry
                        }
                    };
                    candidates.push(candidate);
                }
            }
        }

        candidates.sort();
        candidates
    }

    /// 列出目录项
    fn list_directory_entries(dir_path: &str) -> Result<Vec<String>, i32> {
        let mut buffer = [0u8; 4096];
        let result = listdir(dir_path, &mut buffer);

        if result < 0 {
            return Err(result as i32);
        }

        let mut entries = Vec::new();

        // listdir 返回以换行符分隔的字符串
        if let Ok(contents) = core::str::from_utf8(&buffer[..result as usize]) {
            for line in contents.lines() {
                let line = line.trim();
                if !line.is_empty() && line != "." && line != ".." {
                    entries.push(String::from(line));
                }
            }
        }

        Ok(entries)
    }

    /// 检查文件是否可执行
    fn is_executable(filename: &str, dir: &str) -> bool {
        let full_path = if dir == "." {
            String::from(filename)
        } else {
            format!("{}/{}", dir, filename)
        };

        // 简单检查：尝试打开文件
        let fd = open(&full_path, 0);
        if fd >= 0 {
            close(fd as usize);
            true
        } else {
            false
        }
    }

    /// 检查是否为目录
    fn is_directory(path: &str) -> bool {
        let mut stat_buf = [0u8; 256];
        let result = stat(path, &mut stat_buf);

        if result < 0 {
            return false;
        }

        // 解析stat结构中的mode字段
        if stat_buf.len() >= 4 {
            let mode = u32::from_le_bytes([stat_buf[0], stat_buf[1], stat_buf[2], stat_buf[3]]);
            mode & 0o040000 != 0 // S_IFDIR
        } else {
            false
        }
    }

    /// 显示候选列表
    fn show_candidates(candidates: &[String]) {
        println!("");

        // 计算列数，假设终端宽度80字符
        let term_width = 80;
        let max_len = candidates.iter().map(|s| s.len()).max().unwrap_or(0);
        let col_width = max_len + 2;
        let cols = if col_width > 0 {
            term_width / col_width
        } else {
            1
        };
        let cols = if cols == 0 { 1 } else { cols };

        for (i, candidate) in candidates.iter().enumerate() {
            print!("{:<width$}", candidate, width = col_width);
            if (i + 1) % cols == 0 || i == candidates.len() - 1 {
                println!("");
            }
        }
    }

    /// 找到公共前缀
    fn find_common_prefix(candidates: &[String]) -> String {
        if candidates.is_empty() {
            return String::new();
        }

        if candidates.len() == 1 {
            return candidates[0].clone();
        }

        let first = &candidates[0];
        let mut prefix_len = first.len();

        for candidate in candidates.iter().skip(1) {
            let common_len = first
                .chars()
                .zip(candidate.chars())
                .take_while(|(a, b)| a == b)
                .count();
            prefix_len = prefix_len.min(common_len);
        }

        first.chars().take(prefix_len).collect()
    }

    /// 应用补全结果
    fn apply_completion(
        editor: &mut LineEditor,
        word_start: usize,
        word_end: usize,
        completion: &str,
    ) {
        // 移动光标到单词开始位置
        while editor.cursor_pos() > word_start {
            editor.move_cursor_left();
        }
        while editor.cursor_pos() < word_start {
            editor.move_cursor_right();
        }

        // 删除原有单词
        let chars_to_delete = word_end - word_start;
        for _ in 0..chars_to_delete {
            editor.delete_char_forward();
        }

        // 插入新的补全内容
        for c in completion.chars() {
            editor.insert_char(c);
        }
    }
}
