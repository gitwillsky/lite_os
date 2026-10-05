use core::fmt::{self, Write};
use core::sync::atomic::{AtomicU8, Ordering};

use crate::{println, sync::IrqMutex};

/// 按严重程度递增排列的 kernel log level。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub(crate) enum LogLevel {
    Debug = 0,
    Info = 1,
    Warn = 2,
    Error = 3,
}

impl LogLevel {
    /// 返回 UART 输出使用的 ANSI 着色 level 名称。
    fn colored_str(&self) -> &'static str {
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

/// 一次 `/dev/kmsg` record 读取结果。
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

/// `/dev/kmsg` OFD 独占的 sequence cursor。
pub(crate) struct KmsgReader {
    cursor: IrqMutex<u64>,
}

impl KmsgReader {
    /// 从当前环中最老的仍可读取 record 打开一个独立 reader。
    ///
    /// # Returns
    ///
    /// 不分配的 OFD-local cursor。
    pub(crate) fn open() -> Self {
        Self {
            cursor: IrqMutex::new(LOGGER.lock().oldest_sequence()),
        }
    }

    /// 读取且仅消费一个 Linux `/dev/kmsg` text record。
    ///
    /// # Parameters
    ///
    /// - `output`: kernel-owned 连续缓冲区；不足时 cursor 不前进。
    ///
    /// # Returns
    ///
    /// 完整 record 长度、空、覆盖或 buffer-too-small 状态。
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

    /// 查询当前 cursor 是否落后于 producer 或已发生覆盖。
    ///
    /// # Returns
    ///
    /// 下一次 read 不会返回 Empty 时为 true。
    pub(crate) fn readable(&self) -> bool {
        let cursor = *self.cursor.lock();
        cursor != LOGGER.lock().next_sequence
    }

    /// 返回 producer sequence 作为只读 readiness generation。
    ///
    /// # Returns
    ///
    /// 每发布一条 record 严格递增的 generation。
    pub(crate) fn readiness_generation(&self) -> u64 {
        LOGGER.lock().next_sequence
    }
}

/// kernel log ring 与 UART 输出的唯一 owner。
struct Logger {
    // OWNER: logger 在 UART 输出前同步提交唯一 bounded boot-log ring；若另设 fs/procfs
    // cache，会让 sequence、覆盖与文本内容形成需要人工同步的第二份状态。
    records: [KmsgRecord; KMSG_RECORD_CAPACITY],
    next_sequence: u64,
}

impl Logger {
    const fn new() -> Self {
        Self {
            records: [KmsgRecord::EMPTY; KMSG_RECORD_CAPACITY],
            next_sequence: 0,
        }
    }

    fn oldest_sequence(&self) -> u64 {
        self.next_sequence
            .saturating_sub(KMSG_RECORD_CAPACITY as u64)
    }

    fn log(&mut self, level: LogLevel, module: &str, args: fmt::Arguments) {
        let cpu = crate::cpu::current_id().index();
        let mut message = FixedBytes::<KMSG_MESSAGE_CAPACITY>::new();
        write!(message, "[CPU-{cpu}] [{module}] {args}")
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
            cpu, level, module, args
        );
    }
}

// logger 可由 task、hardirq 和 softirq 调用；普通 spin lock 会在同 CPU 中断重入时自死锁。
// OWNER: log module 独占全局 logger；所有 log macro 经 `__log` 进入同一 ring 与 UART 输出。
static LOGGER: IrqMutex<Logger> = IrqMutex::new(Logger::new());
// OWNER: logging module owns the global severity threshold independently from ring/filter state.
// Missing the macro-side load would evaluate filtered arguments and take LOGGER's IRQ lock.
static LOG_LEVEL: AtomicU8 = AtomicU8::new(LogLevel::Info as u8);

/// 设置全局 severity threshold。
fn set_log_level(level: LogLevel) {
    LOG_LEVEL.store(level as u8, Ordering::Release);
}

/// 在构造 format arguments 前判断 severity threshold。
pub(crate) fn enabled(level: LogLevel) -> bool {
    level as u8 >= LOG_LEVEL.load(Ordering::Acquire)
}

/// log macro 的唯一入口；调用方必须先经 `enabled` 判断 threshold。
pub(crate) fn __log(level: LogLevel, module: &str, args: fmt::Arguments) {
    debug_assert!(enabled(level));
    LOGGER.lock().log(level, module, args);
}

/// Debug level log macro。
#[macro_export]
macro_rules! debug {
    ($($arg:tt)*) => {
        if $crate::log::enabled($crate::log::LogLevel::Debug) {
            $crate::log::__log($crate::log::LogLevel::Debug, module_path!(), format_args!($($arg)*))
        }
    };
}

/// Info level log macro。
#[macro_export]
macro_rules! info {
    ($($arg:tt)*) => {
        if $crate::log::enabled($crate::log::LogLevel::Info) {
            $crate::log::__log($crate::log::LogLevel::Info, module_path!(), format_args!($($arg)*))
        }
    };
}

/// Warn level log macro。
#[macro_export]
macro_rules! warn {
    ($($arg:tt)*) => {
        if $crate::log::enabled($crate::log::LogLevel::Warn) {
            $crate::log::__log($crate::log::LogLevel::Warn, module_path!(), format_args!($($arg)*))
        }
    };
}

/// Error level log macro。
#[macro_export]
macro_rules! error {
    ($($arg:tt)*) => {
        if $crate::log::enabled($crate::log::LogLevel::Error) {
            $crate::log::__log($crate::log::LogLevel::Error, module_path!(), format_args!($($arg)*))
        }
    };
}

/// 按 build profile 初始化 severity threshold：debug 构建输出 Debug，release 构建从 Info 起。
pub(crate) fn init() {
    #[cfg(debug_assertions)]
    set_log_level(LogLevel::Debug);
    #[cfg(not(debug_assertions))]
    set_log_level(LogLevel::Info);
}
