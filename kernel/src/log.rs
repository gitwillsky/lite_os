use core::fmt::{self, Write};

use crate::{println, sync::IrqMutex};

/// Log levels in order of severity
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum LogLevel {
    Debug = 0,
    Info = 1,
    Warn = 2,
    Error = 3,
}

impl LogLevel {
    /// Get the colored string representation of the log level
    pub(crate) fn colored_str(&self) -> &'static str {
        match self {
            LogLevel::Debug => "\x1b[36mDEBUG\x1b[0m", // Cyan
            LogLevel::Info => "\x1b[32mINFO\x1b[0m",   // Green
            LogLevel::Warn => "\x1b[33mWARN\x1b[0m",   // Yellow
            LogLevel::Error => "\x1b[31mERROR\x1b[0m", // Red
        }
    }

    fn syslog_priority(self) -> u8 {
        match self {
            Self::Debug => 7,
            Self::Info => 6,
            Self::Warn => 4,
            Self::Error => 3,
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
const KMSG_RECORD_CAPACITY: usize = 128;
const KMSG_MESSAGE_CAPACITY: usize = 192;
pub(crate) const KMSG_READ_BUFFER_SIZE: usize = 256;

#[derive(Clone, Copy)]
struct KmsgRecord {
    sequence: u64,
    timestamp_us: u64,
    priority: u8,
    length: u8,
    message: [u8; KMSG_MESSAGE_CAPACITY],
}

impl KmsgRecord {
    const EMPTY: Self = Self {
        sequence: 0,
        timestamp_us: 0,
        priority: 0,
        length: 0,
        message: [0; KMSG_MESSAGE_CAPACITY],
    };
}

struct FixedBytes<const N: usize> {
    bytes: [u8; N],
    length: usize,
}

impl<const N: usize> FixedBytes<N> {
    const fn new() -> Self {
        Self {
            bytes: [0; N],
            length: 0,
        }
    }

    fn append(&mut self, bytes: &[u8]) {
        let count = bytes.len().min(N - self.length);
        self.bytes[self.length..self.length + count].copy_from_slice(&bytes[..count]);
        self.length += count;
    }
}

impl<const N: usize> Write for FixedBytes<N> {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        self.append(text.as_bytes());
        Ok(())
    }
}

/// @description 一次 `/dev/kmsg` record 读取结果。
pub(crate) enum KmsgRead {
    /// 一个完整 Linux devkmsg text record。
    Record(usize),
    /// reader 已追上当前 producer sequence。
    Empty,
    /// 环覆盖了 reader 尚未消费的 sequence；下一次读取从当前最老 record 继续。
    Overrun,
    /// caller buffer 无法容纳一个完整 record。
    BufferTooSmall,
}

/// @description `/dev/kmsg` OFD 独占的 sequence cursor。
pub(crate) struct KmsgReader {
    cursor: IrqMutex<u64>,
}

impl KmsgReader {
    /// @description 从当前环中最老的仍可读取 record 打开一个独立 reader。
    /// @return 不分配的 OFD-local cursor。
    pub(crate) fn open() -> Self {
        Self {
            cursor: IrqMutex::new(LOGGER.lock().oldest_sequence()),
        }
    }

    /// @description 读取且仅消费一个 Linux `/dev/kmsg` text record。
    /// @param output kernel-owned 连续缓冲区；不足时 cursor 不前进。
    /// @return 完整 record 长度、空、覆盖或 buffer-too-small 状态。
    pub(crate) fn read(&self, output: &mut [u8]) -> KmsgRead {
        let mut cursor = self.cursor.lock();
        let logger = LOGGER.lock();
        let oldest = logger.oldest_sequence();
        if *cursor < oldest {
            *cursor = oldest;
            return KmsgRead::Overrun;
        }
        if *cursor == logger.next_sequence {
            return KmsgRead::Empty;
        }
        let record = logger.records[*cursor as usize % KMSG_RECORD_CAPACITY];
        assert_eq!(record.sequence, *cursor, "kmsg ring sequence drift");
        let mut wire = FixedBytes::<KMSG_READ_BUFFER_SIZE>::new();
        write!(
            wire,
            "{},{},{},-;",
            record.priority, record.sequence, record.timestamp_us
        )
        .expect("fixed kmsg header formatting failed");
        wire.append(&record.message[..usize::from(record.length)]);
        wire.append(b"\n");
        if output.len() < wire.length {
            return KmsgRead::BufferTooSmall;
        }
        output[..wire.length].copy_from_slice(&wire.bytes[..wire.length]);
        *cursor = (*cursor).checked_add(1).expect("kmsg sequence exhausted");
        KmsgRead::Record(wire.length)
    }

    /// @description 查询当前 cursor 是否落后于 producer 或已发生覆盖。
    /// @return 下一次 read 不会返回 Empty 时为 true。
    pub(crate) fn readable(&self) -> bool {
        let cursor = *self.cursor.lock();
        cursor != LOGGER.lock().next_sequence
    }

    /// @description 返回 producer sequence 作为只读 readiness generation。
    /// @return 每发布一条 record 严格递增的 generation。
    pub(crate) fn readiness_generation(&self) -> u64 {
        LOGGER.lock().next_sequence
    }
}

/// Module filter entry
#[derive(Debug, Clone, Copy)]
pub(crate) struct ModuleFilter {
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
pub(crate) struct Logger {
    level: LogLevel,
    module_filters: [ModuleFilter; MAX_MODULE_FILTERS],
    filter_count: usize,
    default_enabled: bool, // Default state for modules not in filter list
    // OWNER: logger 在 UART 输出前同步提交唯一 bounded boot-log ring；若另设 fs/procfs
    // cache，会让 sequence、覆盖与文本内容形成需要人工同步的第二份状态。
    records: [KmsgRecord; KMSG_RECORD_CAPACITY],
    next_sequence: u64,
}

impl Logger {
    const fn new() -> Self {
        Self {
            level: LogLevel::Info, // Default log level
            module_filters: [ModuleFilter::new(); MAX_MODULE_FILTERS],
            filter_count: 0,
            default_enabled: true, // By default, all modules are enabled
            records: [KmsgRecord::EMPTY; KMSG_RECORD_CAPACITY],
            next_sequence: 0,
        }
    }

    fn oldest_sequence(&self) -> u64 {
        self.next_sequence
            .saturating_sub(KMSG_RECORD_CAPACITY as u64)
    }

    pub(crate) fn set_level(&mut self, level: LogLevel) {
        self.level = level;
    }

    pub(crate) fn disable_module(&mut self, module: &str) -> bool {
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

    pub(crate) fn log(&mut self, level: LogLevel, module: &str, args: fmt::Arguments) {
        if level >= self.level && self.is_module_enabled(module) {
            let hart_id = crate::cpu::current_id().index();
            let mut message = FixedBytes::<KMSG_MESSAGE_CAPACITY>::new();
            write!(message, "[CPU-{hart_id}] [{module}] {args}")
                .expect("fixed kmsg message formatting failed");
            let sequence = self.next_sequence;
            self.records[sequence as usize % KMSG_RECORD_CAPACITY] = KmsgRecord {
                sequence,
                timestamp_us: crate::timer::get_time_us(),
                priority: level.syslog_priority(),
                length: u8::try_from(message.length).expect("kmsg message capacity exceeds u8"),
                message: message.bytes,
            };
            self.next_sequence = sequence.checked_add(1).expect("kmsg sequence exhausted");
            println!(
                "[\x1b[35mCPU-{}\x1b[0m] [{}] [\x1b[34m{}\x1b[0m] {}",
                hart_id, level, module, args
            );
        }
    }
}

// logger 可由 task、hardirq 和 softirq 调用；普通 spin lock 会在同 CPU 中断重入时自死锁。
// OWNER: logging module owns the process-wide logger registered with the log facade.
static LOGGER: IrqMutex<Logger> = IrqMutex::new(Logger::new());

/// Set the global log level
fn set_log_level(level: LogLevel) {
    LOGGER.lock().set_level(level);
}

/// Disable logging for a specific module
pub(crate) fn disable_module(module: &str) -> bool {
    LOGGER.lock().disable_module(module)
}

/// Internal logging function
pub(crate) fn __log(level: LogLevel, module: &str, args: fmt::Arguments) {
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

/// Initialize logging with the build-profile default owned by this module.
pub(crate) fn init() {
    #[cfg(debug_assertions)]
    set_log_level(LogLevel::Debug);
    #[cfg(not(debug_assertions))]
    set_log_level(LogLevel::Info);
}
