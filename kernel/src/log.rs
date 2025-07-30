use core::fmt::{self};
use spin::Mutex;

/// Log levels in order of severity
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogLevel {
    Debug = 0,
    Info = 1,
    Warn = 2,
    Error = 3,
}

impl fmt::Display for LogLevel {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            LogLevel::Debug => write!(f, "DEBUG"),
            LogLevel::Info => write!(f, "INFO"),
            LogLevel::Warn => write!(f, "WARN"),
            LogLevel::Error => write!(f, "ERROR"),
        }
    }
}

/// Global logger configuration
pub struct Logger {
    level: LogLevel,
}

impl Logger {
    const fn new() -> Self {
        Self {
            level: LogLevel::Info, // Default log level
        }
    }

    pub fn set_level(&mut self, level: LogLevel) {
        self.level = level;
    }

    pub fn log(&self, level: LogLevel, module: &str, args: fmt::Arguments) {
        if level >= self.level {
            let hart_id = crate::arch::hart::hart_id();
            println!("[{}] [CORE-{}] [{}] {}", level, hart_id, module, args);
        }
    }
}

static LOGGER: Mutex<Logger> = Mutex::new(Logger::new());

/// Set the global log level
pub fn set_log_level(level: LogLevel) {
    LOGGER.lock().set_level(level);
}

/// Internal logging function
pub fn __log(level: LogLevel, module: &str, args: fmt::Arguments) {
    LOGGER.lock().log(level, module, args);
}

/// Debug level logging macro
#[macro_export]
macro_rules! debug {
    ($($arg:tt)*) => {
        $crate::log::__log($crate::log::LogLevel::Debug, module_path!(), format_args!($($arg)*))
    };
}

/// Info level logging macro
#[macro_export]
macro_rules! info {
    ($($arg:tt)*) => {
        $crate::log::__log($crate::log::LogLevel::Info, module_path!(), format_args!($($arg)*))
    };
}

/// Warning level logging macro
#[macro_export]
macro_rules! warn {
    ($($arg:tt)*) => {
        $crate::log::__log($crate::log::LogLevel::Warn, module_path!(), format_args!($($arg)*))
    };
}

/// Error level logging macro
#[macro_export]
macro_rules! error {
    ($($arg:tt)*) => {
        $crate::log::__log($crate::log::LogLevel::Error, module_path!(), format_args!($($arg)*))
    };
}

/// Initialize logging system with specified level
pub fn init(level: LogLevel) {
    set_log_level(level);
}