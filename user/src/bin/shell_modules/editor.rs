//! 行编辑器模块 - 管理命令行的内容和光标位置

use alloc::string::String;

/// 行编辑器 - 管理命令行的内容和光标位置
pub struct LineEditor {
    /// 命令行内容
    content: String,
    /// 光标在字符串中的位置（字符索引，不是字节索引）
    cursor_pos: usize,
    /// 显示宽度缓存
    display_width: usize,
}

impl LineEditor {
    pub fn new() -> Self {
        Self {
            content: String::new(),
            cursor_pos: 0,
            display_width: 0,
        }
    }

    /// 清空编辑器
    pub fn clear(&mut self) {
        self.content.clear();
        self.cursor_pos = 0;
        self.display_width = 0;
    }

    /// 设置内容（用于历史记录）
    pub fn set_content(&mut self, content: String) {
        self.content = content;
        self.cursor_pos = self.char_count();
        self.display_width = self.calculate_display_width();
    }

    /// 获取内容
    pub fn content(&self) -> &str {
        &self.content
    }

    /// 获取光标位置
    pub fn cursor_pos(&self) -> usize {
        self.cursor_pos
    }

    /// 获取字符数量
    fn char_count(&self) -> usize {
        self.content.chars().count()
    }

    /// 计算显示宽度
    fn calculate_display_width(&self) -> usize {
        let mut width = 0;
        for c in self.content.chars() {
            width += Self::char_display_width(c, width);
        }
        width
    }

    /// 计算字符的显示宽度
    fn char_display_width(c: char, cursor_pos: usize) -> usize {
        match c {
            '\t' => 8 - (cursor_pos % 8),
            _ => 1,
        }
    }

    /// 光标左移
    pub fn move_cursor_left(&mut self) -> bool {
        if self.cursor_pos > 0 {
            self.cursor_pos -= 1;
            true
        } else {
            false
        }
    }

    /// 光标右移
    pub fn move_cursor_right(&mut self) -> bool {
        if self.cursor_pos < self.char_count() {
            self.cursor_pos += 1;
            true
        } else {
            false
        }
    }

    /// 光标移到行首
    pub fn move_cursor_home(&mut self) -> bool {
        if self.cursor_pos > 0 {
            self.cursor_pos = 0;
            true
        } else {
            false
        }
    }

    /// 光标移到行尾
    pub fn move_cursor_end(&mut self) -> bool {
        let char_count = self.char_count();
        if self.cursor_pos < char_count {
            self.cursor_pos = char_count;
            true
        } else {
            false
        }
    }

    /// 在光标位置插入字符
    pub fn insert_char(&mut self, c: char) {
        let byte_pos = self.cursor_to_byte_pos(self.cursor_pos);
        self.content.insert(byte_pos, c);
        self.cursor_pos += 1;
        self.display_width = self.calculate_display_width();
    }

    /// 删除光标前的字符（退格）
    pub fn delete_char_backward(&mut self) -> bool {
        if self.cursor_pos > 0 {
            let byte_pos = self.cursor_to_byte_pos(self.cursor_pos - 1);
            self.content.remove(byte_pos);
            self.cursor_pos -= 1;
            self.display_width = self.calculate_display_width();
            true
        } else {
            false
        }
    }

    /// 删除光标位置的字符（Delete键）
    pub fn delete_char_forward(&mut self) -> bool {
        if self.cursor_pos < self.char_count() {
            let byte_pos = self.cursor_to_byte_pos(self.cursor_pos);
            self.content.remove(byte_pos);
            self.display_width = self.calculate_display_width();
            true
        } else {
            false
        }
    }

    /// 将字符位置转换为字节位置
    fn cursor_to_byte_pos(&self, char_pos: usize) -> usize {
        self.content
            .char_indices()
            .nth(char_pos)
            .map(|(i, _)| i)
            .unwrap_or_else(|| self.content.len())
    }

    /// 计算光标前的显示宽度
    fn cursor_display_width(&self) -> usize {
        let mut width = 0;
        for (i, c) in self.content.chars().enumerate() {
            if i >= self.cursor_pos {
                break;
            }
            width += Self::char_display_width(c, width);
        }
        width
    }

    /// 重新绘制整行
    pub fn redraw_line(&self, prompt: &str) {
        // 移动到行首
        print!("\r");
        // 清除整行
        print!("\x1b[K");
        // 显示提示符和内容
        print!("{}{}", prompt, self.content);
        
        // 计算需要移动的距离
        let total_width = prompt.len() + self.display_width;
        let cursor_width = prompt.len() + self.cursor_display_width();
        
        if cursor_width < total_width {
            // 需要向左移动光标
            let move_left = total_width - cursor_width;
            print!("\x1b[{}D", move_left);
        }
    }
}