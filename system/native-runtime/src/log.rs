//! 写入内核日志的格式化输出。

use core::fmt::{self, Write};

use crate::system::{Severity, debuglog_write};

/// 定长格式化缓冲区：超出单条日志上限的部分截断。
struct Line {
    bytes: [u8; native_abi::DEBUGLOG_MESSAGE_CAPACITY],
    length: usize,
}

impl Write for Line {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let available = self.bytes.len() - self.length;
        let count = text.len().min(available);
        self.bytes[self.length..self.length + count].copy_from_slice(&text.as_bytes()[..count]);
        self.length += count;
        Ok(())
    }
}

/// 格式化并写入一条日志（宏的实现）。
pub fn write(severity: Severity, arguments: fmt::Arguments<'_>) {
    let mut line = Line {
        bytes: [0; native_abi::DEBUGLOG_MESSAGE_CAPACITY],
        length: 0,
    };
    let _ = line.write_fmt(arguments);
    debuglog_write(severity, &line.bytes[..line.length]);
}

/// 写一条 debug 日志。
#[macro_export]
macro_rules! debug {
    ($($argument:tt)*) => {
        $crate::log::write($crate::system::Severity::Debug, format_args!($($argument)*))
    };
}

/// 写一条 info 日志。
#[macro_export]
macro_rules! info {
    ($($argument:tt)*) => {
        $crate::log::write($crate::system::Severity::Info, format_args!($($argument)*))
    };
}

/// 写一条 warning 日志。
#[macro_export]
macro_rules! warn {
    ($($argument:tt)*) => {
        $crate::log::write($crate::system::Severity::Warning, format_args!($($argument)*))
    };
}

/// 写一条 error 日志。
#[macro_export]
macro_rules! error {
    ($($argument:tt)*) => {
        $crate::log::write($crate::system::Severity::Error, format_args!($($argument)*))
    };
}
