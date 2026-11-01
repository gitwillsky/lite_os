//! Linux 内核日志（`/dev/kmsg`、`syslog`）：内核 debuglog 环的 Linux 视图。
//!
//! 1. 记录全部来自内核 debuglog：内核与各系统服务（含 linuxd 自身）写入的日志、以及用户经
//!    `/dev/kmsg` 写入的行，共用同一个序号空间。
//! 2. 读取者持有独立的序号游标，以 `DEBUGLOG_READ` 按序号取记录；被覆盖时报告一次 overrun 并从最老的
//!    可读记录继续。
//! 3. 一个服务线程阻塞等待下一条记录，每有新记录就推进“下一个序号”并发布注册的 deferred 工作，
//!    由它唤醒阻塞的读取者与 poll/epoll。

use core::sync::atomic::{AtomicU64, Ordering};

use crate::sync::Once;
use native_runtime::native_abi::{self, DebuglogRecord};

use crate::deferred::DeferredWork;
use crate::sync::TaskMutex;

pub(crate) mod kmsg_wire;

/// 用户写入未带 `<N>` 前缀时的 level（Linux `default_message_loglevel`）。
const KMSG_USER_DEFAULT_LEVEL: u8 = 4;

/// 一次读取的结果。
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum KmsgRead<E> {
    /// 一条完整记录已交给 `emit`，游标已前进。
    Record,
    /// 游标已追上最新记录。
    Empty,
    /// 环覆盖了尚未读取的记录；下一次从最老记录继续。
    Overrun,
    /// 调用者缓冲放不下一条完整记录；游标不前进。
    BufferTooSmall,
    /// `emit` 失败；游标不前进。
    Emit(E),
    /// 同一打开文件的并发读取者串行化时内存不足。
    OutOfMemory,
}

// OWNER: 下一条尚未出现的记录序号；由观察线程推进，读取者据此判断就绪与跳到末尾。
static NEXT_SEQUENCE: AtomicU64 = AtomicU64::new(0);
// OWNER: 有新记录时发布的 deferred 工作；`/dev/kmsg` 注册时绑定一次。
static PUBLISH_WORK: Once<DeferredWork> = Once::new();

fn read_record(sequence: u64, record: &mut DebuglogRecord) -> Result<(), native_abi::Status> {
    native_runtime::system::debuglog_read(crate::handles::resource(), sequence, record, 0)
}

/// 环中最老的仍可读取的序号。
fn oldest_sequence() -> u64 {
    let mut record = DebuglogRecord::default();
    match read_record(0, &mut record) {
        Err(native_abi::ERR_OUT_OF_RANGE) => record.sequence,
        _ => 0,
    }
}

/// debuglog 级别（0 debug..3 error）到 Linux level。
fn linux_level(severity: u32) -> u8 {
    match severity {
        0 => 7,
        1 => 6,
        2 => 4,
        _ => 3,
    }
}

/// `/dev/kmsg` 打开文件独占的序号游标。
pub(crate) struct KmsgReader {
    cursor: AtomicU64,
    // OWNER: 串行化同一打开文件（dup/fork 共享）的并发读取者；`emit` 写用户内存，不能持自旋锁。
    gate: TaskMutex<()>,
}

impl KmsgReader {
    /// 从最老的可读记录开始。
    pub(crate) fn open() -> Self {
        Self {
            cursor: AtomicU64::new(oldest_sequence()),
            gate: TaskMutex::new(()),
        }
    }

    /// 读取并消费恰好一条记录（Linux `devkmsg_read` 文本格式）。
    pub(crate) fn read<E>(
        &self,
        capacity: usize,
        emit: &mut dyn FnMut(&[u8]) -> Result<(), E>,
    ) -> KmsgRead<E> {
        let Ok(_gate) = self.gate.lock() else {
            return KmsgRead::OutOfMemory;
        };
        let cursor = self.cursor.load(Ordering::Acquire);
        let mut record = DebuglogRecord::default();
        match read_record(cursor, &mut record) {
            Ok(()) => {}
            Err(native_abi::ERR_OUT_OF_RANGE) => {
                self.cursor.store(record.sequence, Ordering::Release);
                return KmsgRead::Overrun;
            }
            Err(_) => return KmsgRead::Empty,
        }
        let length = (record.length as usize).min(record.message.len());
        match kmsg_wire::encode(
            linux_level(record.severity),
            record.sequence,
            record.timestamp_ns / 1000,
            &record.message[..length],
            capacity,
            emit,
        ) {
            Ok(kmsg_wire::Encoded::Done) => {
                self.cursor.store(cursor + 1, Ordering::Release);
                KmsgRead::Record
            }
            Ok(kmsg_wire::Encoded::TooSmall) => KmsgRead::BufferTooSmall,
            Err(error) => KmsgRead::Emit(error),
        }
    }

    /// 下一次读取不会返回 `Empty`。
    pub(crate) fn readable(&self) -> bool {
        self.cursor.load(Ordering::Acquire) < NEXT_SEQUENCE.load(Ordering::Acquire)
    }

    /// 回到最老的可读记录（`SEEK_SET`/`SEEK_DATA`）。
    pub(crate) fn seek_oldest(&self) {
        self.cursor.store(oldest_sequence(), Ordering::Release);
    }

    /// 跳到末尾，只读取此后的记录（`SEEK_END`/`SEEK_HOLE`）。
    pub(crate) fn seek_newest(&self) {
        self.cursor
            .store(NEXT_SEQUENCE.load(Ordering::Acquire), Ordering::Release);
    }

    /// 每出现一条记录严格递增的就绪代号。
    pub(crate) fn readiness_generation(&self) -> u64 {
        NEXT_SEQUENCE.load(Ordering::Acquire)
    }
}

/// 绑定有新记录时发布的 deferred 工作，并启动观察线程。
///
/// # Panics
///
/// 观察线程创建失败：没有它阻塞的读取者永远不会被唤醒。
pub(crate) fn bind_publish_work(work: DeferredWork) {
    PUBLISH_WORK.call_once(|| work);
    let watcher = native_runtime::thread::spawn(b"kmsg-watch", || {
        let mut record = DebuglogRecord::default();
        let mut next = oldest_sequence();
        loop {
            match native_runtime::system::debuglog_read(
                crate::handles::resource(),
                next,
                &mut record,
                native_abi::DEADLINE_INFINITE,
            ) {
                Ok(()) => next = record.sequence + 1,
                Err(native_abi::ERR_OUT_OF_RANGE) => {
                    next = record.sequence;
                    continue;
                }
                Err(_) => continue,
            }
            NEXT_SEQUENCE.fetch_max(next, Ordering::AcqRel);
            if let Some(work) = PUBLISH_WORK.get() {
                crate::deferred::raise(*work);
            }
        }
    })
    .expect("kmsg watcher thread");
    drop(watcher);
}

/// 把一次 `/dev/kmsg` 写入作为日志记录发布（Linux `devkmsg_write`）：`<N>` 前缀给出 level。
pub(crate) fn publish_user_message(input: &[u8]) {
    let (priority, text) = kmsg_wire::parse_user_message(input, KMSG_USER_DEFAULT_LEVEL);
    let severity = match priority & 7 {
        0..=3 => native_runtime::system::Severity::Error,
        4 => native_runtime::system::Severity::Warning,
        5 | 6 => native_runtime::system::Severity::Info,
        _ => native_runtime::system::Severity::Debug,
    };
    native_runtime::system::debuglog_write(severity, text);
}
