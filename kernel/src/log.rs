use core::fmt::{self, Write};
use spin::Mutex;
use alloc::string::String;

const LOG_BUFFER_SIZE: usize = 1024;

pub struct StackBuffer {
    buffer: [u8; LOG_BUFFER_SIZE],
    position: usize,
}

impl StackBuffer {
    pub fn new() -> Self {
        Self {
            buffer: [0; LOG_BUFFER_SIZE],
            position: 0,
        }
    }

    pub fn as_str(&self) -> &str {
        unsafe {
            core::str::from_utf8_unchecked(&self.buffer[..self.position])
        }
    }
}

impl Write for StackBuffer {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let bytes = s.as_bytes();
        let remaining = LOG_BUFFER_SIZE - self.position;

        if bytes.len() <= remaining {
            self.buffer[self.position..self.position + bytes.len()].copy_from_slice(bytes);
            self.position += bytes.len();
            Ok(())
        } else {
            // Truncate if buffer is full
            if remaining > 0 {
                self.buffer[self.position..].copy_from_slice(&bytes[..remaining]);
                self.position = LOG_BUFFER_SIZE;
            }
            Err(fmt::Error)
        }
    }
}

/// ANSI color codes for terminal output
pub struct Colors;

impl Colors {
    // Reset to default color
    pub const RESET: &'static str = "\x1b[0m";

    // Foreground colors
    pub const BLACK: &'static str = "\x1b[30m";
    pub const RED: &'static str = "\x1b[31m";
    pub const GREEN: &'static str = "\x1b[32m";
    pub const YELLOW: &'static str = "\x1b[33m";
    pub const BLUE: &'static str = "\x1b[34m";
    pub const MAGENTA: &'static str = "\x1b[35m";
    pub const CYAN: &'static str = "\x1b[36m";
    pub const WHITE: &'static str = "\x1b[37m";

    // Bright foreground colors
    pub const BRIGHT_BLACK: &'static str = "\x1b[90m";
    pub const BRIGHT_RED: &'static str = "\x1b[91m";
    pub const BRIGHT_GREEN: &'static str = "\x1b[92m";
    pub const BRIGHT_YELLOW: &'static str = "\x1b[93m";
    pub const BRIGHT_BLUE: &'static str = "\x1b[94m";
    pub const BRIGHT_MAGENTA: &'static str = "\x1b[95m";
    pub const BRIGHT_CYAN: &'static str = "\x1b[96m";
    pub const BRIGHT_WHITE: &'static str = "\x1b[97m";

    // Text styles
    pub const BOLD: &'static str = "\x1b[1m";
    pub const DIM: &'static str = "\x1b[2m";
    pub const ITALIC: &'static str = "\x1b[3m";
    pub const UNDERLINE: &'static str = "\x1b[4m";
}

/// Log levels in order of severity
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogLevel {
    Debug = 0,
    Info = 1,
    Warn = 2,
    Error = 3,
}

impl LogLevel {
    /// Get the color code for this log level
    pub fn color(&self) -> &'static str {
        match self {
            LogLevel::Debug => Colors::CYAN,
            LogLevel::Info => Colors::GREEN,
            LogLevel::Warn => Colors::YELLOW,
            LogLevel::Error => Colors::RED,
        }
    }

    /// Get the bright color code for this log level
    pub fn bright_color(&self) -> &'static str {
        match self {
            LogLevel::Debug => Colors::BRIGHT_CYAN,
            LogLevel::Info => Colors::BRIGHT_GREEN,
            LogLevel::Warn => Colors::BRIGHT_YELLOW,
            LogLevel::Error => Colors::BRIGHT_RED,
        }
    }

    /// Get the name of this log level
    pub fn name(&self) -> &'static str {
        match self {
            LogLevel::Debug => "DEBUG",
            LogLevel::Info => "INFO",
            LogLevel::Warn => "WARN",
            LogLevel::Error => "ERROR",
        }
    }
}

impl fmt::Display for LogLevel {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.name())
    }
}

/// Logger configuration
#[derive(Debug, Clone, Copy)]
pub struct LoggerConfig {
    /// Minimum log level to display
    pub level: LogLevel,
    /// Whether to use colors in output
    pub enable_colors: bool,
    /// Whether to use bright colors
    pub use_bright_colors: bool,
    /// Whether to show timestamps
    pub show_timestamps: bool,
    /// Whether to show CPU ID
    pub show_cpu_id: bool,
}

impl Default for LoggerConfig {
    fn default() -> Self {
        Self {
            level: LogLevel::Info,
            enable_colors: true,
            use_bright_colors: false,
            show_timestamps: false,
            show_cpu_id: true,
        }
    }
}

/// Global logger configuration
pub struct Logger {
    config: LoggerConfig,
}

impl Logger {
    const fn new() -> Self {
        Self {
            config: LoggerConfig {
                level: LogLevel::Info,
                enable_colors: true,
                use_bright_colors: false,
                show_timestamps: false,
                show_cpu_id: true,
            },
        }
    }

    pub fn set_level(&mut self, level: LogLevel) {
        self.config.level = level;
    }

    pub fn set_config(&mut self, config: LoggerConfig) {
        self.config = config;
    }

    pub fn enable_colors(&mut self, enable: bool) {
        self.config.enable_colors = enable;
    }

    pub fn use_bright_colors(&mut self, use_bright: bool) {
        self.config.use_bright_colors = use_bright;
    }

    pub fn log(&self, level: LogLevel, module: &str, args: fmt::Arguments) {
        if level >= self.config.level {
            // Use stack buffer to avoid heap allocation
            let mut buffer = StackBuffer::new();

            // Add timestamp if enabled
            if self.config.show_timestamps {
                let time_us = crate::timer::get_time_us();
                let _ = write!(buffer, "[{:>8}.{:03}] ", time_us / 1000, time_us % 1000);
            }

            // Add CPU ID if enabled (skip during early boot to avoid hangs)
            if self.config.show_cpu_id {
                let cpu_id = crate::smp::current_cpu_id();
                let _ = write!(buffer, "[CPU{}] ", cpu_id);
            }

            // Add colored log level
            if self.config.enable_colors {
                let color = if self.config.use_bright_colors {
                    level.bright_color()
                } else {
                    level.color()
                };
                let _ = write!(buffer, "[{}{}{}] ", color, level.name(), Colors::RESET);
            } else {
                let _ = write!(buffer, "[{}] ", level.name());
            }

            // Add module name with color
            if self.config.enable_colors {
                let _ = write!(buffer, "[{}{}{}] ", Colors::DIM, module, Colors::RESET);
            } else {
                let _ = write!(buffer, "[{}] ", module);
            }

            // Add the actual log message
            let _ = write!(buffer, "{}", args);

            // Print the complete formatted message
            println!("{}", buffer.as_str());
        }
    }
}

static LOGGER: Mutex<Logger> = Mutex::new(Logger::new());

/// Set the global log level
pub fn set_log_level(level: LogLevel) {
    LOGGER.lock().set_level(level);
}

/// Set the complete logger configuration
pub fn set_log_config(config: LoggerConfig) {
    LOGGER.lock().set_config(config);
}

/// Enable or disable colored output
pub fn enable_colors(enable: bool) {
    LOGGER.lock().enable_colors(enable);
}

/// Use bright colors for log levels
pub fn use_bright_colors(use_bright: bool) {
    LOGGER.lock().use_bright_colors(use_bright);
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

/// Initialize logging system with full configuration
pub fn init_with_config(config: LoggerConfig) {
    set_log_config(config);
}

/// Initialize logging system with colors enabled/disabled
pub fn init_with_colors(level: LogLevel, enable_colors: bool) {
    let mut config = LoggerConfig::default();
    config.level = level;
    config.enable_colors = enable_colors;
    set_log_config(config);
}

/// Auto-detect color support and initialize accordingly
/// This is a simple heuristic - in a real system you might check TERM environment variable
pub fn init_auto() {
    let config = LoggerConfig {
        level: LogLevel::Info,
        enable_colors: true, // Enable colors by default - can be disabled if needed
        use_bright_colors: false,
        show_timestamps: false,
        show_cpu_id: true,
    };
    set_log_config(config);
}