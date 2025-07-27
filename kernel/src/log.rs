use core::fmt::{self, Write};
use spin::Mutex;

const LOG_BUFFER_SIZE: usize = 4096;

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
#[derive(Debug, Clone)]
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
    /// Module filter for controlling which modules can log
    pub module_filter: ModuleFilter,
}

impl Default for LoggerConfig {
    fn default() -> Self {
        Self {
            level: LogLevel::Info,
            enable_colors: true,
            use_bright_colors: false,
            show_timestamps: false,
            show_cpu_id: true,
            module_filter: ModuleFilter::new(),
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
                module_filter: ModuleFilter::new(),
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
        if level >= self.config.level && self.config.module_filter.is_module_enabled(module) {
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

/// Enable logging for a specific module or module prefix (early boot safe)
/// Note: This function only works with predefined static module patterns
/// For common modules, use the predefined constants like MODULE_SYSCALL, MODULE_MEMORY, etc.
pub fn enable_module_pattern(pattern: &'static str) -> bool {
    LOGGER.lock().config.module_filter.enable_module(pattern)
}

/// Disable logging for a specific module or module prefix (early boot safe)
/// Note: This function only works with predefined static module patterns
/// For common modules, use the predefined constants like MODULE_SYSCALL, MODULE_MEMORY, etc.
pub fn disable_module_pattern(pattern: &'static str) -> bool {
    LOGGER.lock().config.module_filter.disable_module(pattern)
}

/// Set the module filter to allow all modules (default behavior)
pub fn enable_all_modules() {
    LOGGER.lock().config.module_filter = ModuleFilter::allow_all();
}

/// Set the module filter to block all modules by default
/// After calling this, you need to explicitly enable modules you want to see
pub fn disable_all_modules() {
    LOGGER.lock().config.module_filter = ModuleFilter::block_all();
}

/// Clear all module filters and reset to default (all modules enabled)
pub fn clear_module_filters() {
    LOGGER.lock().config.module_filter.clear();
}

/// Print current module filter configuration
pub fn print_module_filter_info() {
    let logger = LOGGER.lock();
    let filter = &logger.config.module_filter;

    println!("=== Module Filter Configuration ===");
    println!("Default enabled: {}", filter.default_enabled);

    let enabled_count = filter.get_enabled_count();
    let disabled_count = filter.get_disabled_count();

    if enabled_count > 0 {
        println!("Enabled modules:");
        for i in 0..enabled_count {
            if let Some(module) = filter.get_enabled_module(i) {
                println!("  + {}", module);
            }
        }
    }

    if disabled_count > 0 {
        println!("Disabled modules:");
        for i in 0..disabled_count {
            if let Some(module) = filter.get_disabled_module(i) {
                println!("  - {}", module);
            }
        }
    }

    if enabled_count == 0 && disabled_count == 0 {
        if filter.default_enabled {
            println!("All modules are enabled (default)");
        } else {
            println!("All modules are disabled (default)");
        }
    }
    println!("==================================");
}

// Predefined module patterns (static strings for early boot safety)
pub const MODULE_SYSCALL: &str = "kernel::syscall";
pub const MODULE_MEMORY: &str = "kernel::memory";
pub const MODULE_FS: &str = "kernel::fs";
pub const MODULE_TASK: &str = "kernel::task";
pub const MODULE_SMP: &str = "kernel::smp";
pub const MODULE_TRAP: &str = "kernel::trap";
pub const MODULE_DRIVERS: &str = "kernel::drivers";
pub const MODULE_TIMER: &str = "kernel::timer";

// Specific filesystem modules
pub const MODULE_FAT32: &str = "kernel::fs::fat32";
pub const MODULE_VFS: &str = "kernel::fs::vfs";

// Specific driver modules
pub const MODULE_VIRTIO_BLK: &str = "kernel::drivers::virtio_blk";
pub const MODULE_VIRTIO_CONSOLE: &str = "kernel::drivers::virtio_console";

// Convenience functions for common modules
pub fn enable_syscall_logs() -> bool { enable_module_pattern(MODULE_SYSCALL) }
pub fn disable_syscall_logs() -> bool { disable_module_pattern(MODULE_SYSCALL) }
pub fn enable_memory_logs() -> bool { enable_module_pattern(MODULE_MEMORY) }
pub fn disable_memory_logs() -> bool { disable_module_pattern(MODULE_MEMORY) }
pub fn enable_fs_logs() -> bool { enable_module_pattern(MODULE_FS) }
pub fn disable_fs_logs() -> bool { disable_module_pattern(MODULE_FS) }

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
        module_filter: ModuleFilter::new(),
    };
    set_log_config(config);
}

/// Maximum number of module patterns that can be stored
const MAX_MODULE_PATTERNS: usize = 32;

/// Module filter for controlling which modules can log (no heap allocation)
#[derive(Debug, Clone)]
pub struct ModuleFilter {
    /// Enabled module patterns (supports prefix matching)
    enabled_modules: [Option<&'static str>; MAX_MODULE_PATTERNS],
    /// Disabled module patterns (takes precedence over enabled)
    disabled_modules: [Option<&'static str>; MAX_MODULE_PATTERNS],
    /// Number of enabled patterns
    enabled_count: usize,
    /// Number of disabled patterns
    disabled_count: usize,
    /// If true, all modules are enabled by default (whitelist mode)
    /// If false, all modules are disabled by default (blacklist mode)
    default_enabled: bool,
}

impl ModuleFilter {
    pub const fn new() -> Self {
        Self {
            enabled_modules: [None; MAX_MODULE_PATTERNS],
            disabled_modules: [None; MAX_MODULE_PATTERNS],
            enabled_count: 0,
            disabled_count: 0,
            default_enabled: true, // By default, all modules are enabled
        }
    }

    /// Create a filter that allows all modules
    pub const fn allow_all() -> Self {
        Self {
            enabled_modules: [None; MAX_MODULE_PATTERNS],
            disabled_modules: [None; MAX_MODULE_PATTERNS],
            enabled_count: 0,
            disabled_count: 0,
            default_enabled: true,
        }
    }

    /// Create a filter that blocks all modules by default
    pub const fn block_all() -> Self {
        Self {
            enabled_modules: [None; MAX_MODULE_PATTERNS],
            disabled_modules: [None; MAX_MODULE_PATTERNS],
            enabled_count: 0,
            disabled_count: 0,
            default_enabled: false,
        }
    }

    /// Enable logging for a specific module or module prefix
    /// Uses static string literals to avoid heap allocation
    pub fn enable_module(&mut self, module: &'static str) -> bool {
        // Remove from disabled first
        self.remove_disabled_module(module);

        // Add to enabled if not already present and space available
        for i in 0..self.enabled_count {
            if let Some(existing) = self.enabled_modules[i] {
                if existing == module {
                    return true; // Already enabled
                }
            }
        }

        if self.enabled_count < MAX_MODULE_PATTERNS {
            self.enabled_modules[self.enabled_count] = Some(module);
            self.enabled_count += 1;
            true
        } else {
            false // No space available
        }
    }

    /// Disable logging for a specific module or module prefix
    /// Uses static string literals to avoid heap allocation
    pub fn disable_module(&mut self, module: &'static str) -> bool {
        // Remove from enabled first
        self.remove_enabled_module(module);

        // Add to disabled if not already present and space available
        for i in 0..self.disabled_count {
            if let Some(existing) = self.disabled_modules[i] {
                if existing == module {
                    return true; // Already disabled
                }
            }
        }

        if self.disabled_count < MAX_MODULE_PATTERNS {
            self.disabled_modules[self.disabled_count] = Some(module);
            self.disabled_count += 1;
            true
        } else {
            false // No space available
        }
    }

    /// Check if a module is allowed to log
    pub fn is_module_enabled(&self, module: &str) -> bool {
        // Check if explicitly disabled (takes priority)
        for i in 0..self.disabled_count {
            if let Some(disabled_pattern) = self.disabled_modules[i] {
                if module.starts_with(disabled_pattern) {
                    return false;
                }
            }
        }

        // Check if explicitly enabled
        for i in 0..self.enabled_count {
            if let Some(enabled_pattern) = self.enabled_modules[i] {
                if module.starts_with(enabled_pattern) {
                    return true;
                }
            }
        }

        // Use default behavior
        self.default_enabled
    }

    /// Clear all filters and reset to default
    pub fn clear(&mut self) {
        self.enabled_modules = [None; MAX_MODULE_PATTERNS];
        self.disabled_modules = [None; MAX_MODULE_PATTERNS];
        self.enabled_count = 0;
        self.disabled_count = 0;
        self.default_enabled = true;
    }

    /// Remove a module from enabled list
    fn remove_enabled_module(&mut self, module: &'static str) {
        for i in 0..self.enabled_count {
            if let Some(existing) = self.enabled_modules[i] {
                if existing == module {
                    // Shift remaining elements down
                    for j in i..self.enabled_count - 1 {
                        self.enabled_modules[j] = self.enabled_modules[j + 1];
                    }
                    self.enabled_modules[self.enabled_count - 1] = None;
                    self.enabled_count -= 1;
                    break;
                }
            }
        }
    }

    /// Remove a module from disabled list
    fn remove_disabled_module(&mut self, module: &'static str) {
        for i in 0..self.disabled_count {
            if let Some(existing) = self.disabled_modules[i] {
                if existing == module {
                    // Shift remaining elements down
                    for j in i..self.disabled_count - 1 {
                        self.disabled_modules[j] = self.disabled_modules[j + 1];
                    }
                    self.disabled_modules[self.disabled_count - 1] = None;
                    self.disabled_count -= 1;
                    break;
                }
            }
        }
    }

    /// Get count of enabled module patterns
    pub fn get_enabled_count(&self) -> usize {
        self.enabled_count
    }

    /// Get count of disabled module patterns
    pub fn get_disabled_count(&self) -> usize {
        self.disabled_count
    }

    /// Get enabled module pattern by index
    pub fn get_enabled_module(&self, index: usize) -> Option<&'static str> {
        if index < self.enabled_count {
            self.enabled_modules[index]
        } else {
            None
        }
    }

    /// Get disabled module pattern by index
    pub fn get_disabled_module(&self, index: usize) -> Option<&'static str> {
        if index < self.disabled_count {
            self.disabled_modules[index]
        } else {
            None
        }
    }
}

impl Default for ModuleFilter {
    fn default() -> Self {
        Self::new()
    }
}