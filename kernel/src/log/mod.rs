//! 内核 debuglog：有界日志环与调试串口输出。
//!
//! 日志记录同时进入环与调试串口；系统服务经 native `debuglog_read` 按序号读取原始记录，
//! 自行决定对外格式。调试串口输入由独立的 `serial` 领域拥有。

use core::fmt::{self, Write};
use core::sync::atomic::{AtomicU8, Ordering};
use spin::Once;

use crate::{
    execution::deferred::DeferredWork,
    execution::sync::{IrqMutex, WaitQueue, WaitResult},
    println,
};


/// 按严重程度递增排列的日志级别。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub(crate) enum LogLevel {
    Debug = 0,
    Info = 1,
    Warn = 2,
    Error = 3,
}

impl LogLevel {
    /// 调试串口输出使用的 ANSI 着色名称。
    fn colored_str(&self) -> &'static str {
        match self {
            LogLevel::Debug => "\x1b[36mDEBUG\x1b[0m",
            LogLevel::Info => "\x1b[32mINFO\x1b[0m",
            LogLevel::Warn => "\x1b[33mWARN\x1b[0m",
            LogLevel::Error => "\x1b[31mERROR\x1b[0m",
        }
    }

    /// 由 native ABI 的数值构造；越界按 Error 处理。
    pub(crate) fn from_raw(value: u8) -> Self {
        match value {
            0 => Self::Debug,
            1 => Self::Info,
            2 => Self::Warn,
            _ => Self::Error,
        }
    }
}

impl fmt::Display for LogLevel {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.colored_str())
    }
}

const RECORD_CAPACITY: usize = 128;
/// 单条记录的消息上限；更长的写入被截断。
pub(crate) const MESSAGE_CAPACITY: usize = 1024;

#[derive(Clone, Copy)]
struct Record {
    sequence: u64,
    timestamp_us: u64,
    level: LogLevel,
    length: u16,
    message: [u8; MESSAGE_CAPACITY],
}

impl Record {
    const EMPTY: Self = Self {
        sequence: 0,
        timestamp_us: 0,
        level: LogLevel::Debug,
        length: 0,
        message: [0; MESSAGE_CAPACITY],
    };
}

/// 一条日志记录的元数据与消息。
pub(crate) struct RecordCopy {
    pub(crate) sequence: u64,
    pub(crate) timestamp_us: u64,
    pub(crate) level: LogLevel,
    pub(crate) length: usize,
    pub(crate) message: [u8; MESSAGE_CAPACITY],
}

/// 按序号读取一条记录的结果。
pub(crate) enum ReadRecord {
    /// 读到序号 `cursor` 的记录。
    Record(alloc::boxed::Box<RecordCopy>),
    /// `cursor` 已被覆盖；最老的可读序号。
    Overrun(u64),
    /// 尚无该序号的记录。
    Empty,
}

/// 向定长槽位追加文本；写满后静默截断。
struct SliceBytes<'a> {
    bytes: &'a mut [u8],
    length: usize,
}

impl Write for SliceBytes<'_> {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let count = text.len().min(self.bytes.len() - self.length);
        self.bytes[self.length..self.length + count].copy_from_slice(&text.as_bytes()[..count]);
        self.length += count;
        Ok(())
    }
}

/// 把任意字节按 UTF-8 显示，非法序列显示为 U+FFFD。
struct Lossy<'a>(&'a [u8]);

impl fmt::Display for Lossy<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for chunk in self.0.utf8_chunks() {
            formatter.write_str(chunk.valid())?;
            if !chunk.invalid().is_empty() {
                formatter.write_str("\u{fffd}")?;
            }
        }
        Ok(())
    }
}

/// 日志环与调试串口输出的唯一 owner。
struct Logger {
    // OWNER: 唯一有界日志环；输出前同步提交，序号、覆盖与文本只有这一份状态。
    records: [Record; RECORD_CAPACITY],
    next_sequence: u64,
}

impl Logger {
    const fn new() -> Self {
        Self {
            records: [Record::EMPTY; RECORD_CAPACITY],
            next_sequence: 0,
        }
    }

    fn oldest_sequence(&self) -> u64 {
        self.next_sequence.saturating_sub(RECORD_CAPACITY as u64)
    }

    /// 发布一条系统服务写入的记录；消息直接写入环槽位（调用者可能在中断栈上）。
    fn log_external(&mut self, level: LogLevel, text: &[u8]) {
        let length = text.len().min(MESSAGE_CAPACITY);
        let sequence = self.next_sequence;
        let slot = &mut self.records[sequence as usize % RECORD_CAPACITY];
        slot.sequence = sequence;
        slot.timestamp_us = crate::execution::timer::get_time_us();
        slot.level = level;
        slot.length = length as u16;
        slot.message[..length].copy_from_slice(&text[..length]);
        self.next_sequence = sequence.checked_add(1).expect("log sequence exhausted");
    }

    fn log(&mut self, level: LogLevel, module: &str, args: fmt::Arguments) {
        let cpu = crate::execution::cpu::current_id().index();
        let sequence = self.next_sequence;
        // 直接格式化进环槽位：hardirq 也会走到这里，栈上不放 1 KiB 临时缓冲。
        let slot = &mut self.records[sequence as usize % RECORD_CAPACITY];
        let mut message = SliceBytes {
            bytes: &mut slot.message,
            length: 0,
        };
        write!(message, "[CPU-{cpu}] [{module}] {args}").expect("slice formatting never fails");
        let length = message.length;
        slot.sequence = sequence;
        slot.timestamp_us = crate::execution::timer::get_time_us();
        slot.level = level;
        slot.length = length as u16;
        self.next_sequence = sequence.checked_add(1).expect("log sequence exhausted");
        println!(
            "[\x1b[35mCPU-{}\x1b[0m] [{}] [\x1b[34m{}\x1b[0m] {}",
            cpu, level, module, args
        );
    }
}

// logger 可由 task、hardirq 和 softirq 调用；普通 spin lock 会在同 CPU 中断重入时自死锁。
// OWNER: 全局唯一 logger；所有日志宏与 native 写入都进入同一环与串口输出。
static LOGGER: IrqMutex<Logger> = IrqMutex::new(Logger::new());
// OWNER: logging module owns the global severity threshold independently from ring/filter state.
// Missing the macro-side load would evaluate filtered arguments and take LOGGER's IRQ lock.
static LOG_LEVEL: AtomicU8 = AtomicU8::new(LogLevel::Info as u8);

/// 把日志环最近 `count` 条记录 以 `[秒.微秒] 文本` 写入 `out`，供崩溃现场使用。
///
/// 锁忙时只写明现场不可用，不等待或强行解锁；未响应停止 IPI 的 CPU 仍可能访问旧 guard。
///
/// # Parameters
/// - `out`: 接收文本的输出端。
/// - `count`: 最多输出的条数，不超过环容量。
///
/// # Returns
/// 无返回值；输出错误按 fail-stop 的尽力而为语义忽略。
pub(crate) fn write_tail_for_fail_stop(out: &mut dyn fmt::Write, count: usize) {
    let Some(logger) = LOGGER.try_lock() else {
        let _ = writeln!(out, "kernel log unavailable: logger lock is held");
        return;
    };
    let first = logger
        .next_sequence
        .saturating_sub(count.min(RECORD_CAPACITY) as u64);
    for sequence in first..logger.next_sequence {
        let record = &logger.records[sequence as usize % RECORD_CAPACITY];
        if record.sequence != sequence || usize::from(record.length) > MESSAGE_CAPACITY {
            continue;
        }
        let _ = writeln!(
            out,
            "[{:5}.{:06}] {}",
            record.timestamp_us / 1_000_000,
            record.timestamp_us % 1_000_000,
            Lossy(&record.message[..usize::from(record.length)])
        );
    }
}

/// 设置全局 severity threshold。
fn set_log_level(level: LogLevel) {
    LOG_LEVEL.store(level as u8, Ordering::Release);
}

/// 按内核启动参数 `loglevel=N` 设置输出阈值：N ≥ 8 输出 Debug，7 输出 Info，5..6 输出 Warn，其余只输出 Error。
pub(crate) fn apply_console_loglevel(level: u8) {
    set_log_level(match level {
        8.. => LogLevel::Debug,
        7 => LogLevel::Info,
        5 | 6 => LogLevel::Warn,
        _ => LogLevel::Error,
    });
}

/// 在构造 format arguments 前判断 severity threshold。
pub(crate) fn enabled(level: LogLevel) -> bool {
    level as u8 >= LOG_LEVEL.load(Ordering::Acquire)
}

/// log macro 的唯一入口；调用方必须先经 `enabled` 判断 threshold。
pub(crate) fn __log(level: LogLevel, module: &str, args: fmt::Arguments) {
    debug_assert!(enabled(level));
    LOGGER.lock().log(level, module, args);
    publish_notification();
}

// OWNER: 日志读者的唤醒向量；logger 可在中断与任何持锁上下文调用，不能直接唤醒线程，
// 只能发布这个 deferred vector。缺失时阻塞读取的系统服务永远收不到新记录。
static PUBLISH_WORK: Once<DeferredWork> = Once::new();
// OWNER: 阻塞等待新记录的读者。
static READERS: WaitQueue = WaitQueue::new();

/// 注册唤醒读者的 deferred vector；在 deferred 拓扑初始化后调用一次。
///
/// # Panics
/// vector 耗尽时 fail-stop。
pub(crate) fn initialize_readers() {
    let work = crate::execution::deferred::register(|_| {
        READERS.wake_all();
        false
    })
    .expect("debuglog deferred vector exhausted");
    PUBLISH_WORK.call_once(|| work);
}

fn publish_notification() {
    if let Some(work) = PUBLISH_WORK.get() {
        crate::execution::deferred::raise(*work);
    }
}

/// 发布一条系统服务写入的记录；通过阈值时同时输出到调试串口。
pub(crate) fn write_external(level: LogLevel, text: &[u8]) {
    let cpu = crate::execution::cpu::current_id().index();
    {
        let mut logger = LOGGER.lock();
        logger.log_external(level, text);
        if enabled(level) {
            // 输出与入环在同一把锁内，保证串口与环的顺序一致。
            println!(
                "[\x1b[35mCPU-{}\x1b[0m] [{}] [\x1b[34muser\x1b[0m] {}",
                cpu,
                level,
                Lossy(&text[..text.len().min(MESSAGE_CAPACITY)])
            );
        }
    }
    publish_notification();
}

/// 读取序号为 `cursor` 的记录；`deadline_ns` 为 `Some` 时在没有记录时等待。
///
/// # Errors
/// 等待超时、被打断或分配失败时返回对应 [`WaitResult`]。
pub(crate) fn read_record(cursor: u64, deadline_ns: Option<u64>) -> Result<ReadRecord, WaitResult> {
    let mut copy = alloc::boxed::Box::try_new(RecordCopy {
        sequence: 0,
        timestamp_us: 0,
        level: LogLevel::Debug,
        length: 0,
        message: [0; MESSAGE_CAPACITY],
    })
    .map_err(|_| WaitResult::OutOfMemory)?;
    loop {
        {
            let logger = LOGGER.lock();
            let oldest = logger.oldest_sequence();
            if cursor < oldest {
                return Ok(ReadRecord::Overrun(oldest));
            }
            if cursor < logger.next_sequence {
                let record = &logger.records[cursor as usize % RECORD_CAPACITY];
                copy.sequence = record.sequence;
                copy.timestamp_us = record.timestamp_us;
                copy.level = record.level;
                copy.length = usize::from(record.length);
                copy.message.copy_from_slice(&record.message);
                return Ok(ReadRecord::Record(copy));
            }
        }
        let Some(deadline) = deadline_ns else {
            return Ok(ReadRecord::Empty);
        };
        match READERS.wait_until(|| LOGGER.lock().next_sequence > cursor, Some(deadline)) {
            WaitResult::Woken => {}
            other => return Err(other),
        }
    }
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
