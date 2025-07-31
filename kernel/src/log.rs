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

impl LogLevel {
    /// Get the colored string representation of the log level
    pub fn colored_str(&self) -> &'static str {
        match self {
            LogLevel::Debug => "\x1b[36mDEBUG\x1b[0m", // Cyan
            LogLevel::Info => "\x1b[32mINFO\x1b[0m",   // Green
            LogLevel::Warn => "\x1b[33mWARN\x1b[0m",   // Yellow
            LogLevel::Error => "\x1b[31mERROR\x1b[0m", // Red
        }
    }
}

impl fmt::Display for LogLevel {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.colored_str())
    }
}

/// Maximum number of module filters
const MAX_MODULE_FILTERS: usize = 32;

/// Module filter entry
#[derive(Debug, Clone, Copy)]
pub struct ModuleFilter {
    name: [u8; 32], // Fixed-size module name
    name_len: usize,
    enabled: bool,
}

impl ModuleFilter {
    const fn new() -> Self {
        Self {
            name: [0; 32],
            name_len: 0,
            enabled: true,
        }
    }

    fn set_name(&mut self, name: &str) {
        let bytes = name.as_bytes();
        let len = core::cmp::min(bytes.len(), 31); // Leave space for null terminator
        self.name[..len].copy_from_slice(&bytes[..len]);
        self.name[len] = 0; // Null terminator
        self.name_len = len;
    }

    fn matches(&self, module: &str) -> bool {
        if self.name_len == 0 {
            return false;
        }
        let module_bytes = module.as_bytes();
        if module_bytes.len() != self.name_len {
            return false;
        }
        &self.name[..self.name_len] == module_bytes
    }
}

/// Global logger configuration
pub struct Logger {
    level: LogLevel,
    module_filters: [ModuleFilter; MAX_MODULE_FILTERS],
    filter_count: usize,
    default_enabled: bool, // Default state for modules not in filter list
}

impl Logger {
    const fn new() -> Self {
        Self {
            level: LogLevel::Info, // Default log level
            module_filters: [ModuleFilter::new(); MAX_MODULE_FILTERS],
            filter_count: 0,
            default_enabled: true, // By default, all modules are enabled
        }
    }

    pub fn set_level(&mut self, level: LogLevel) {
        self.level = level;
    }

    pub fn set_default_enabled(&mut self, enabled: bool) {
        self.default_enabled = enabled;
    }

    pub fn enable_module(&mut self, module: &str) -> bool {
        // First check if module already exists in filters
        for i in 0..self.filter_count {
            if self.module_filters[i].matches(module) {
                self.module_filters[i].enabled = true;
                return true;
            }
        }

        // Add new filter if space available
        if self.filter_count < MAX_MODULE_FILTERS {
            self.module_filters[self.filter_count].set_name(module);
            self.module_filters[self.filter_count].enabled = true;
            self.filter_count += 1;
            true
        } else {
            false // No space for more filters
        }
    }

    pub fn disable_module(&mut self, module: &str) -> bool {
        // First check if module already exists in filters
        for i in 0..self.filter_count {
            if self.module_filters[i].matches(module) {
                self.module_filters[i].enabled = false;
                return true;
            }
        }

        // Add new filter if space available
        if self.filter_count < MAX_MODULE_FILTERS {
            self.module_filters[self.filter_count].set_name(module);
            self.module_filters[self.filter_count].enabled = false;
            self.filter_count += 1;
            true
        } else {
            false // No space for more filters
        }
    }

    fn is_module_enabled(&self, module: &str) -> bool {
        // Check if module has specific filter
        for i in 0..self.filter_count {
            if self.module_filters[i].matches(module) {
                return self.module_filters[i].enabled;
            }
        }
        // Use default state if no specific filter found
        self.default_enabled
    }

    pub fn log(&self, level: LogLevel, module: &str, args: fmt::Arguments) {
        if level >= self.level && self.is_module_enabled(module) {
            let hart_id = crate::arch::hart::hart_id();
            println!("[\x1b[35mCPU-{}\x1b[0m] [{}] [\x1b[34m{}\x1b[0m] {}",
                     hart_id, level, module, args);
        }
    }
}

static LOGGER: Mutex<Logger> = Mutex::new(Logger::new());

/// Set the global log level
pub fn set_log_level(level: LogLevel) {
    LOGGER.lock().set_level(level);
}

/// Set the default enabled state for modules not in filter list
pub fn set_default_module_enabled(enabled: bool) {
    LOGGER.lock().set_default_enabled(enabled);
}

/// Enable logging for a specific module
pub fn enable_module(module: &str) -> bool {
    LOGGER.lock().enable_module(module)
}

/// Disable logging for a specific module
pub fn disable_module(module: &str) -> bool {
    LOGGER.lock().disable_module(module)
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

/// Initialize logging system with level and module filtering
pub fn init_with_module_filter(level: LogLevel, default_enabled: bool) {
    set_log_level(level);
    set_default_module_enabled(default_enabled);
}