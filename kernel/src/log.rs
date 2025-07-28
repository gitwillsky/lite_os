use core::fmt::{self, Write};
use spin::Mutex;

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

/// Simple stack-based log formatting
struct StackLogBuffer {
    buffer: [u8; 512],
    pos: usize,
}

impl StackLogBuffer {
    fn new() -> Self {
        Self {
            buffer: [0; 512],
            pos: 0,
        }
    }

    fn write_str(&mut self, s: &str) {
        let bytes = s.as_bytes();
        let space = self.buffer.len() - self.pos;
        let to_copy = bytes.len().min(space);
        
        if to_copy > 0 {
            self.buffer[self.pos..self.pos + to_copy].copy_from_slice(&bytes[..to_copy]);
            self.pos += to_copy;
        }
    }

    fn write_char(&mut self, c: char) {
        if self.pos < self.buffer.len() {
            self.buffer[self.pos] = c as u8;
            self.pos += 1;
        }
    }

    fn write_number(&mut self, mut num: usize) {
        if num == 0 {
            self.write_char('0');
            return;
        }

        let mut digits = [0u8; 20]; // enough for 64-bit numbers
        let mut count = 0;

        while num > 0 {
            digits[count] = (b'0' + (num % 10) as u8);
            num /= 10;
            count += 1;
        }

        // Write digits in reverse order
        for i in (0..count).rev() {
            self.write_char(digits[i] as char);
        }
    }

    fn as_str(&self) -> &str {
        core::str::from_utf8(&self.buffer[..self.pos]).unwrap_or("[INVALID_UTF8]")
    }
}

/// Simplified logger for direct console output
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

    /// Simplified logging with direct console output
    pub fn log(&self, level: LogLevel, module: &str, args: fmt::Arguments) {
        if level >= self.config.level && self.config.module_filter.is_module_enabled(module) {
            let mut buffer = StackLogBuffer::new();

            // Add CPU ID if enabled
            if self.config.show_cpu_id {
                let cpu_id = crate::smp::current_cpu_id();
                buffer.write_str("[CPU");
                buffer.write_number(cpu_id);
                buffer.write_str("] ");
            }

            // Add log level with colors
            if self.config.enable_colors {
                let color = if self.config.use_bright_colors {
                    level.bright_color()
                } else {
                    level.color()
                };
                buffer.write_str("[");
                buffer.write_str(color);
                buffer.write_str(level.name());
                buffer.write_str(Colors::RESET);
                buffer.write_str("] ");
            } else {
                buffer.write_str("[");
                buffer.write_str(level.name());
                buffer.write_str("] ");
            }

            // Add module name
            if self.config.enable_colors {
                buffer.write_str("[");
                buffer.write_str(Colors::DIM);
                buffer.write_str(module);
                buffer.write_str(Colors::RESET);
                buffer.write_str("] ");
            } else {
                buffer.write_str("[");
                buffer.write_str(module);
                buffer.write_str("] ");
            }

            // Format and add message
            if let Some(simple_msg) = args.as_str() {
                buffer.write_str(simple_msg);
            } else {
                // Format into temporary buffer
                let mut temp = [0u8; 256];
                let mut cursor = 0;
                
                struct TempWriter<'a> {
                    buffer: &'a mut [u8],
                    cursor: &'a mut usize,
                }
                
                impl<'a> Write for TempWriter<'a> {
                    fn write_str(&mut self, s: &str) -> fmt::Result {
                        let bytes = s.as_bytes();
                        let space = self.buffer.len() - *self.cursor;
                        let to_copy = bytes.len().min(space);
                        
                        if to_copy > 0 {
                            self.buffer[*self.cursor..*self.cursor + to_copy]
                                .copy_from_slice(&bytes[..to_copy]);
                            *self.cursor += to_copy;
                        }
                        Ok(())
                    }
                }
                
                let mut writer = TempWriter { buffer: &mut temp, cursor: &mut cursor };
                if writer.write_fmt(args).is_ok() && cursor > 0 {
                    if let Ok(s) = core::str::from_utf8(&temp[..cursor]) {
                        buffer.write_str(s);
                    }
                }
            }

            // Output directly to console
            if level == LogLevel::Error {
                crate::console::emergency_print(buffer.as_str());
            } else {
                crate::console::print_direct(buffer.as_str());
            }
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
pub fn enable_module_pattern(pattern: &'static str) -> bool {
    LOGGER.lock().config.module_filter.enable_module(pattern)
}

/// Disable logging for a specific module or module prefix (early boot safe)
pub fn disable_module_pattern(pattern: &'static str) -> bool {
    LOGGER.lock().config.module_filter.disable_module(pattern)
}

/// Set the module filter to allow all modules (default behavior)
pub fn enable_all_modules() {
    LOGGER.lock().config.module_filter = ModuleFilter::allow_all();
}

/// Set the module filter to block all modules by default
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

    // Use emergency print to ensure this diagnostic info is always shown
    crate::console::emergency_print("=== Module Filter Configuration ===\n");

    if filter.default_enabled {
        crate::console::emergency_print("Default enabled: true\n");
    } else {
        crate::console::emergency_print("Default enabled: false\n");
    }

    let enabled_count = filter.get_enabled_count();
    let disabled_count = filter.get_disabled_count();

    if enabled_count > 0 {
        crate::console::emergency_print("Enabled modules:\n");
        for i in 0..enabled_count {
            if let Some(module) = filter.get_enabled_module(i) {
                crate::console::emergency_print("  + ");
                crate::console::emergency_print(module);
                crate::console::emergency_print("\n");
            }
        }
    }

    if disabled_count > 0 {
        crate::console::emergency_print("Disabled modules:\n");
        for i in 0..disabled_count {
            if let Some(module) = filter.get_disabled_module(i) {
                crate::console::emergency_print("  - ");
                crate::console::emergency_print(module);
                crate::console::emergency_print("\n");
            }
        }
    }

    if enabled_count == 0 && disabled_count == 0 {
        if filter.default_enabled {
            crate::console::emergency_print("All modules are enabled (default)\n");
        } else {
            crate::console::emergency_print("All modules are disabled (default)\n");
        }
    }
    crate::console::emergency_print("==================================\n");
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

/// Error level logging macro (uses emergency mode for critical errors)
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
pub fn init_auto() {
    let config = LoggerConfig {
        level: LogLevel::Info,
        enable_colors: true,
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