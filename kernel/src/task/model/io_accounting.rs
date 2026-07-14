use core::sync::atomic::{AtomicU64, Ordering};

use super::TaskControlBlock;

/// @description Linux task I/O accounting 的不可变读取快照。
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct IoStatistics {
    pub(crate) read_characters: u64,
    pub(crate) written_characters: u64,
    pub(crate) read_syscalls: u64,
    pub(crate) write_syscalls: u64,
    pub(crate) read_bytes: u64,
    pub(crate) write_bytes: u64,
}

/// @description 一个 Thread 或 Process 的唯一并发 I/O counter owner。
///
/// Thread 与 Process 各有独立 Linux 统计口径：每次完成路径同步递增当前 Thread 与所属
/// Process；缺少 Process owner 会在 worker 退出后丢失 `/proc/<tgid>/io` 历史。
#[derive(Debug, Default)]
pub(super) struct IoAccounting {
    read_characters: AtomicU64,
    written_characters: AtomicU64,
    read_syscalls: AtomicU64,
    write_syscalls: AtomicU64,
    read_bytes: AtomicU64,
    write_bytes: AtomicU64,
}

impl IoAccounting {
    pub(super) fn account_read_result(&self, result: isize) {
        self.read_syscalls.fetch_add(1, Ordering::Relaxed);
        if let Ok(bytes) = usize::try_from(result) {
            self.read_characters
                .fetch_add(bytes as u64, Ordering::Relaxed);
        }
    }

    pub(super) fn account_write_result(&self, result: isize) {
        self.write_syscalls.fetch_add(1, Ordering::Relaxed);
        if let Ok(bytes) = usize::try_from(result) {
            self.written_characters
                .fetch_add(bytes as u64, Ordering::Relaxed);
        }
    }

    pub(super) fn account_read_storage(&self, bytes: usize) {
        self.read_bytes.fetch_add(bytes as u64, Ordering::Relaxed);
    }

    pub(super) fn account_write_storage(&self, bytes: usize) {
        self.write_bytes.fetch_add(bytes as u64, Ordering::Relaxed);
    }

    pub(super) fn snapshot(&self) -> IoStatistics {
        IoStatistics {
            read_characters: self.read_characters.load(Ordering::Relaxed),
            written_characters: self.written_characters.load(Ordering::Relaxed),
            read_syscalls: self.read_syscalls.load(Ordering::Relaxed),
            write_syscalls: self.write_syscalls.load(Ordering::Relaxed),
            read_bytes: self.read_bytes.load(Ordering::Relaxed),
            write_bytes: self.write_bytes.load(Ordering::Relaxed),
        }
    }
}

impl TaskControlBlock {
    /// @description 记录一次成功 read-family operation 的 logical byte 与 syscall 计数。
    ///
    /// @param result Linux byte result 或 operation errno；只有非负结果推进 rchar。
    /// @return 无返回值；当前 Thread 与 Process 聚合 owner 同步推进。
    pub(crate) fn account_read_result(&self, result: isize) {
        self.thread.io_accounting.account_read_result(result);
        self.process.io_accounting.account_read_result(result);
    }

    /// @description 记录一次成功 write-family operation 的 logical byte 与 syscall 计数。
    ///
    /// @param result Linux byte result 或 operation errno；只有非负结果推进 wchar。
    /// @return 无返回值；当前 Thread 与 Process 聚合 owner 同步推进。
    pub(crate) fn account_write_result(&self, result: isize) {
        self.thread.io_accounting.account_write_result(result);
        self.process.io_accounting.account_write_result(result);
    }

    /// @description 记录本次 regular read 实际触发 cache-miss storage fill 的字节数。
    ///
    /// @param bytes filesystem storage owner 成功读取的字节数。
    /// @return 无返回值；cache hit 必须传零，防止把 logical read 冒充 block I/O。
    pub(crate) fn account_read_storage(&self, bytes: usize) {
        self.thread.io_accounting.account_read_storage(bytes);
        self.process.io_accounting.account_read_storage(bytes);
    }

    /// @description 记录本次 synchronous regular write 实际提交给 storage 的字节数。
    ///
    /// @param bytes filesystem storage owner 成功写入的字节数。
    /// @return 无返回值；partial write 只累计已提交前缀。
    pub(crate) fn account_write_storage(&self, bytes: usize) {
        self.thread.io_accounting.account_write_storage(bytes);
        self.process.io_accounting.account_write_storage(bytes);
    }

    /// @description 取得当前 Thread 的 Linux I/O counter 快照。
    /// @return `/proc/<tgid>/task/<tid>/io` 使用的当前值。
    pub(in crate::task) fn thread_io_statistics(&self) -> IoStatistics {
        self.thread.io_accounting.snapshot()
    }

    /// @description 取得当前 Process 全生命周期聚合 I/O counter 快照。
    /// @return `/proc/<tgid>/io` 使用的当前值，包含已退出 Thread。
    pub(in crate::task) fn process_io_statistics(&self) -> IoStatistics {
        self.process.io_accounting.snapshot()
    }
}
