//! 命令历史记录模块

use alloc::collections::VecDeque;
use alloc::string::String;

pub struct CommandHistory {
    commands: VecDeque<String>,
    current_index: isize,
    max_size: usize,
}

impl CommandHistory {
    pub fn new(max_size: usize) -> Self {
        CommandHistory {
            commands: VecDeque::new(),
            current_index: -1,
            max_size,
        }
    }

    pub fn add_command(&mut self, command: String) {
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

    pub fn get_previous(&mut self) -> Option<&String> {
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

    pub fn get_next(&mut self) -> Option<&String> {
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
