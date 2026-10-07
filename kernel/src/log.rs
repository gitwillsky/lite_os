use core::fmt::{self, Write};
use core::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use spin::Once;

use crate::{
    cpu::DeferredWork,
    println,
    sync::{IrqMutex, TaskMutex},
};

#[path = "log/kmsg_wire.rs"]
mod kmsg_wire;

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
/// 单条消息上限，与 Linux `LOG_LINE_MAX` 相同；更长的用户写入被截断。
const KMSG_MESSAGE_CAPACITY: usize = 1024;
/// 内核自身产生的 record 使用 facility 0（`LOG_KERN`）。
const KMSG_USER_DEFAULT_LEVEL: u8 = 4;

#[derive(Clone, Copy)]
struct KmsgRecord {
    sequence: u64,
    timestamp_us: u64,
    /// `facility << 3 | level`。
    priority: u8,
    length: u16,
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

/// 向定长槽位追加文本；写满后静默截断（与 Linux 对超长消息的处理一致）。
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

/// 一次 `/dev/kmsg` record 读取结果。
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum KmsgRead<E> {
    /// 一个完整 record 已交给 `emit`，cursor 已前进。
    Record,
    /// reader 已追上当前 producer sequence。
    Empty,
    /// 环覆盖了 reader 尚未消费的 sequence；下一次读取从当前最老 record 继续。
    Overrun,
    /// caller buffer 无法容纳一个完整 record；cursor 不前进。
    BufferTooSmall,
    /// `emit` 失败；cursor 不前进，record 下次读取时重新交出。
    Emit(E),
    /// 序列化同一 OFD 的并发 reader 时等待元数据分配失败。
    OutOfMemory,
}

/// `/dev/kmsg` OFD 独占的 sequence cursor。
pub(crate) struct KmsgReader {
    // 下一条要交出的 sequence。只在持有 `gate` 时修改；poll 与 seek 只需要无锁读取最新值。
    cursor: AtomicU64,
    // OWNER: 序列化同一个 OFD（dup/fork 共享）的并发 reader。`emit` 会写用户内存并可能缺页，
    // 所以不能用 IRQ/spin lock；缺失时两个 reader 会各自读到同一条 record 并各自前进 cursor，
    // 丢掉下一条。
    gate: TaskMutex<()>,
}

impl KmsgReader {
    /// 从当前环中最老的仍可读取 record 打开一个独立 reader。
    ///
    /// # Returns
    ///
    /// 不分配的 OFD-local cursor。
    pub(crate) fn open() -> Self {
        Self {
            cursor: AtomicU64::new(LOGGER.lock().oldest_sequence()),
            gate: TaskMutex::new(()),
        }
    }

    /// 读取且仅消费一个 Linux `/dev/kmsg` text record。
    ///
    /// 1. 在 logger 锁内只复制该条 record，随即释放锁；
    /// 2. 在锁外编码并经 `emit` 交给调用者（可能触发缺页）；
    /// 3. 全部交出后才推进 cursor，所以任何失败都不会丢 record。
    ///
    /// # Parameters
    ///
    /// - `capacity`: 调用者缓冲字节数；放不下整条 record 时返回 `BufferTooSmall`。
    /// - `emit`: 接收编码后的连续分片。
    ///
    /// # Returns
    ///
    /// 见 [`KmsgRead`]。
    pub(crate) fn read<E>(
        &self,
        capacity: usize,
        emit: &mut dyn FnMut(&[u8]) -> Result<(), E>,
    ) -> KmsgRead<E> {
        let Ok(_gate) = self.gate.lock() else {
            return KmsgRead::OutOfMemory;
        };
        let cursor = self.cursor.load(Ordering::Acquire);
        let record = {
            let logger = LOGGER.lock();
            let oldest = logger.oldest_sequence();
            if cursor < oldest {
                self.cursor.store(oldest, Ordering::Release);
                return KmsgRead::Overrun;
            }
            if cursor == logger.next_sequence {
                return KmsgRead::Empty;
            }
            let record = logger.records[cursor as usize % KMSG_RECORD_CAPACITY];
            assert_eq!(record.sequence, cursor, "kmsg ring sequence drift");
            record
        };
        match kmsg_wire::encode(
            record.priority,
            record.sequence,
            record.timestamp_us,
            &record.message[..usize::from(record.length)],
            capacity,
            emit,
        ) {
            Ok(kmsg_wire::Encoded::Done) => {
                self.cursor.store(
                    cursor.checked_add(1).expect("kmsg sequence exhausted"),
                    Ordering::Release,
                );
                KmsgRead::Record
            }
            Ok(kmsg_wire::Encoded::TooSmall) => KmsgRead::BufferTooSmall,
            Err(error) => KmsgRead::Emit(error),
        }
    }

    /// 查询当前 cursor 是否落后于 producer 或已发生覆盖。
    ///
    /// # Returns
    ///
    /// 下一次 read 不会返回 Empty 时为 true。
    pub(crate) fn readable(&self) -> bool {
        self.cursor.load(Ordering::Acquire) != LOGGER.lock().next_sequence
    }

    /// 移到环中最老的仍可读取 record（`SEEK_SET`/`SEEK_DATA`）。
    pub(crate) fn seek_oldest(&self) {
        self.cursor
            .store(LOGGER.lock().oldest_sequence(), Ordering::Release);
    }

    /// 移到 producer 之后，只读取此后发布的 record（`SEEK_END`/`SEEK_HOLE`）。
    pub(crate) fn seek_newest(&self) {
        self.cursor
            .store(LOGGER.lock().next_sequence, Ordering::Release);
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

    /// 发布一条用户经 `/dev/kmsg` 写入的 record（Linux `devkmsg_write` → `printk_emit`）。
    ///
    /// record 总是进入环；是否同时输出到 UART 由调用者按 console loglevel 决定。消息直接写入环槽位，
    /// 不经栈上临时副本：调用者可能在 hardirq 栈上。
    fn log_user(&mut self, priority: u8, text: &[u8]) {
        let length = text.len().min(KMSG_MESSAGE_CAPACITY);
        let sequence = self.next_sequence;
        let slot = &mut self.records[sequence as usize % KMSG_RECORD_CAPACITY];
        slot.sequence = sequence;
        slot.timestamp_us = crate::timer::get_time_us();
        slot.priority = priority;
        slot.length = length as u16;
        slot.message[..length].copy_from_slice(&text[..length]);
        self.next_sequence = sequence.checked_add(1).expect("kmsg sequence exhausted");
    }

    fn log(&mut self, level: LogLevel, module: &str, args: fmt::Arguments) {
        let cpu = crate::cpu::current_id().index();
        let sequence = self.next_sequence;
        // 直接格式化进环槽位：hardirq 也会走到这里，栈上不放 1 KiB 临时缓冲。
        let slot = &mut self.records[sequence as usize % KMSG_RECORD_CAPACITY];
        let mut message = SliceBytes {
            bytes: &mut slot.message,
            length: 0,
        };
        write!(message, "[CPU-{cpu}] [{module}] {args}").expect("slice formatting never fails");
        let length = message.length;
        slot.sequence = sequence;
        slot.timestamp_us = crate::timer::get_time_us();
        slot.priority = level.syslog_priority();
        slot.length = length as u16;
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

/// 按 Linux console loglevel（`loglevel=`、`quiet`=4、`debug`=10）设置 severity threshold：
/// level N 输出严重度数值小于 N 的消息（ERR=3、WARNING=4、INFO=6、DEBUG=7）。
///
/// 没有比 Error 更高的级别，N ≤ 3 按只输出 Error 处理；threshold 同时作用于 kmsg ring。
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

// OWNER: `/dev/kmsg` 的阻塞 reader 唤醒通道，由 fs 在启动期绑定一次。logger 可在 hardirq 与任何持锁
// 上下文调用，不能在其中直接唤醒任务（会取 scheduler 锁），只能发布这个 deferred vector；缺失时
// 阻塞在 `/dev/kmsg` 上的 reader 永远收不到新 record。
static PUBLISH_WORK: Once<DeferredWork> = Once::new();

/// 绑定 record 发布后要触发的 deferred vector；只能调用一次。
pub(crate) fn bind_publish_work(work: DeferredWork) {
    PUBLISH_WORK.call_once(|| work);
}

/// 在 logger 锁之外发布一次“有新 record”的合并通知。
fn publish_notification() {
    if let Some(work) = PUBLISH_WORK.get() {
        crate::cpu::raise_deferred(*work);
    }
}

/// 把一次 `/dev/kmsg` 写入作为 record 发布（Linux `devkmsg_write`）。
///
/// 解析 `<N>` 前缀得到 facility/level，去掉结尾换行；record 总是进入环，仅当 level 通过 console
/// loglevel 时才同时输出到 UART。
///
/// # Parameters
///
/// - `input`: 用户写入的原始字节（调用者已按 `LOG_LINE_MAX` 截断）。
pub(crate) fn publish_user_message(input: &[u8]) {
    let (priority, text) = kmsg_wire::parse_user_message(input, KMSG_USER_DEFAULT_LEVEL);
    let level = match priority & 7 {
        0..=3 => LogLevel::Error,
        4 => LogLevel::Warn,
        5 | 6 => LogLevel::Info,
        _ => LogLevel::Debug,
    };
    let cpu = crate::cpu::current_id().index();
    {
        let mut logger = LOGGER.lock();
        logger.log_user(priority, text);
        if enabled(level) {
            // 输出与入环在同一把锁内，保证 UART 与环的顺序一致（与内核 record 相同）。
            println!(
                "[\x1b[35mCPU-{}\x1b[0m] [{}] [\x1b[34muser\x1b[0m] {}",
                cpu,
                level,
                Lossy(&text[..text.len().min(KMSG_MESSAGE_CAPACITY)])
            );
        }
    }
    publish_notification();
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
