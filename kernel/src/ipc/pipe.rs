use core::sync::atomic;

use crate::{
    fs::{
        FileSystemError,
        inode::{Inode, InodeType},
    },
    task::{TaskControlBlock, block_current_and_run_next, current_task},
};
use alloc::{
    collections::{BTreeMap, VecDeque},
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use spin::Mutex;

/// 管道缓冲区大小
const PIPE_BUF_SIZE: usize = 4096;

/// 管道结构体
pub struct Pipe {
    /// 数据缓冲区和状态
    inner: Mutex<PipeInner>,
}

struct PipeInner {
    /// 数据缓冲区
    buffer: VecDeque<u8>,
    /// 读端是否关闭
    read_closed: bool,
    /// 写端是否关闭
    write_closed: bool,
    /// 等待读取的任务队列
    read_wait_queue: Vec<Weak<TaskControlBlock>>,
    /// 等待写入的任务队列
    write_wait_queue: Vec<Weak<TaskControlBlock>>,
    /// 等待写入器连接的读取器队列（用于FIFO）
    read_open_wait_queue: Vec<Weak<TaskControlBlock>>,
    /// 等待读取器连接的写入器队列（用于FIFO）
    write_open_wait_queue: Vec<Weak<TaskControlBlock>>,
    /// poll 等待者：pid -> (弱引用任务, 关心的事件掩码)
    poll_waiters: BTreeMap<usize, (Weak<TaskControlBlock>, u32)>,
}

impl Pipe {
    /// 创建新的管道
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(PipeInner {
                buffer: VecDeque::with_capacity(PIPE_BUF_SIZE),
                read_closed: false,
                write_closed: false,
                read_wait_queue: Vec::new(),
                write_wait_queue: Vec::new(),
                read_open_wait_queue: Vec::new(),
                write_open_wait_queue: Vec::new(),
                poll_waiters: BTreeMap::new(),
            }),
        }
    }

    /// 关闭读端
    pub fn close_read(&self) {
        let mut inner = self.inner.lock();
        inner.read_closed = true;
        // 唤醒所有等待写入的任务
        Self::wakeup_waiters(&mut inner.write_wait_queue);
    }

    /// 关闭写端
    pub fn close_write(&self) {
        let mut inner = self.inner.lock();
        inner.write_closed = true;
        // 唤醒所有等待读取的任务
        Self::wakeup_waiters(&mut inner.read_wait_queue);
    }

    /// 通知有新的写入器连接（用于FIFO）
    pub fn notify_writer_connected(&self) {
        let mut inner = self.inner.lock();
        // 唤醒所有等待写入器连接的读取器
        Self::wakeup_waiters(&mut inner.read_open_wait_queue);
    }

    /// 通知有新的读取器连接（用于FIFO）
    pub fn notify_reader_connected(&self) {
        let mut inner = self.inner.lock();
        // 唤醒所有等待读取器连接的写入器
        Self::wakeup_waiters(&mut inner.write_open_wait_queue);
    }

    /// 唤醒等待队列中的任务
    fn wakeup_waiters(wait_queue: &mut Vec<Weak<TaskControlBlock>>) {
        let tasks_to_wakeup: Vec<_> = wait_queue
            .drain(..)
            .filter_map(|weak_task| weak_task.upgrade())
            .collect();

        // 在不持有锁的情况下唤醒任务
        for task in tasks_to_wakeup {
            task.wakeup();
        }
    }

    fn wakeup_pollers(&self, mask: u32) {
        let mut to_wakeup: Vec<Weak<TaskControlBlock>> = Vec::new();
        {
            let mut inner = self.inner.lock();
            let mut dead: Vec<usize> = Vec::new();
            for (pid, (weak, interests)) in inner.poll_waiters.iter() {
                if (mask & *interests) != 0 {
                    to_wakeup.push(weak.clone());
                }
                if weak.upgrade().is_none() {
                    dead.push(*pid);
                }
            }
            for pid in dead {
                inner.poll_waiters.remove(&pid);
            }
        }
        for w in to_wakeup {
            if let Some(task) = w.upgrade() {
                task.wakeup();
            }
        }
    }

    /// 计算常规管道的 poll 就绪掩码
    pub fn poll_mask(&self) -> u32 {
        // 与 sys_poll 中的常量保持一致
        const POLLIN: u32 = 0x0001;
        const POLLOUT: u32 = 0x0004;
        const POLLHUP: u32 = 0x0010;
        let inner = self.inner.lock();
        let mut mask = 0u32;
        if !inner.buffer.is_empty() {
            mask |= POLLIN;
        }
        if inner.buffer.len() < PIPE_BUF_SIZE && !inner.read_closed {
            mask |= POLLOUT;
        }
        if inner.read_closed || inner.write_closed {
            mask |= POLLHUP;
        }
        mask
    }

    /// 从管道读取数据（阻塞式）
    pub fn read(&self, buf: &mut [u8]) -> Result<usize, FileSystemError> {
        loop {
            // 尝试读取数据
            let read_result = {
                let mut inner = self.inner.lock();

                if !inner.buffer.is_empty() {
                    // 有数据可读
                    let read_len = buf.len().min(inner.buffer.len());
                    for i in 0..read_len {
                        buf[i] = inner.buffer.pop_front().unwrap();
                    }

                    // 唤醒等待写入的任务
                    Self::wakeup_waiters(&mut inner.write_wait_queue);
                    drop(inner);
                    // POLLIN 触发
                    self.wakeup_pollers(0x0001);

                    Some(Ok(read_len))
                } else if inner.write_closed {
                    // 写端关闭且无数据，返回EOF
                    Some(Ok(0))
                } else {
                    // 无数据且写端未关闭，需要阻塞等待
                    if let Some(current) = current_task() {
                        inner.read_wait_queue.push(Arc::downgrade(&current));
                        None // 表示需要阻塞
                    } else {
                        Some(Err(FileSystemError::IoError))
                    }
                }
            };

            match read_result {
                Some(result) => return result,
                None => {
                    // 需要阻塞等待
                    block_current_and_run_next();
                    // 任务被唤醒后继续循环检查
                }
            }
        }
    }

    /// 向管道写入数据（阻塞式）
    pub fn write(&self, buf: &[u8]) -> Result<usize, FileSystemError> {
        if buf.is_empty() {
            return Ok(0);
        }

        let mut total_written = 0;
        let mut remaining = buf;

        while !remaining.is_empty() {
            // 尝试写入数据
            let write_result = {
                let mut inner = self.inner.lock();

                if inner.read_closed {
                    // 读端关闭，写入失败 (SIGPIPE)
                    Some(Err(FileSystemError::PermissionDenied))
                } else {
                    let available_space = PIPE_BUF_SIZE - inner.buffer.len();
                    if available_space > 0 {
                        // 有空间可写
                        let write_len = remaining.len().min(available_space);
                        for i in 0..write_len {
                            inner.buffer.push_back(remaining[i]);
                        }

                        total_written += write_len;
                        remaining = &remaining[write_len..];

                        // 唤醒等待读取的任务
                        Self::wakeup_waiters(&mut inner.read_wait_queue);
                        drop(inner);
                        // POLLOUT 触发
                        self.wakeup_pollers(0x0004);

                        Some(Ok(()))
                    } else {
                        // 缓冲区满
                        if total_written > 0 {
                            // 已有部分写入，本次调用到此为止，返回已写入字节数
                            return Ok(total_written);
                        } else {
                            // 尚未写入任何数据，直接返回0，避免单线程死锁
                            return Ok(0);
                        }
                    }
                }
            };

            match write_result {
                Some(Ok(())) => continue, // 继续写入剩余数据
                Some(Err(e)) => return Err(e),
                None => {
                    // 需要阻塞等待
                    block_current_and_run_next();
                    // 任务被唤醒后继续循环检查
                }
            }
        }

        Ok(total_written)
    }

    /// FIFO读取方法，需要检查写入器连接状态
    pub fn fifo_read(&self, buf: &mut [u8], has_writers: bool) -> Result<usize, FileSystemError> {
        loop {
            // 尝试读取数据
            let read_result = {
                let mut inner = self.inner.lock();

                if !inner.buffer.is_empty() {
                    // 有数据可读
                    let read_len = buf.len().min(inner.buffer.len());
                    for i in 0..read_len {
                        buf[i] = inner.buffer.pop_front().unwrap();
                    }

                    // 唤醒等待写入的任务
                    Self::wakeup_waiters(&mut inner.write_wait_queue);

                    Some(Ok(read_len))
                } else if inner.write_closed {
                    // 写端关闭且无数据，返回EOF
                    Some(Ok(0))
                } else {
                    // 无数据：
                    // - 如果当前没有写入器连接（has_writers == false），按FIFO语义应当阻塞等待写入器连接
                    // - 如果已有写入器但暂时无数据，同样阻塞等待数据
                    if let Some(current) = current_task() {
                        inner.read_wait_queue.push(Arc::downgrade(&current));
                        None // 表示需要阻塞
                    } else {
                        Some(Err(FileSystemError::IoError))
                    }
                }
            };

            match read_result {
                Some(result) => return result,
                None => {
                    // 需要阻塞等待
                    block_current_and_run_next();
                    // 任务被唤醒后继续循环检查
                }
            }
        }
    }

    /// FIFO写入方法，需要检查读取器连接状态
    pub fn fifo_write(&self, buf: &[u8], has_readers: bool) -> Result<usize, FileSystemError> {
        if buf.is_empty() {
            return Ok(0);
        }

        if !has_readers {
            // 没有读取器连接，写入失败 (SIGPIPE)
            return Err(FileSystemError::PermissionDenied);
        }

        let mut total_written = 0;
        let mut remaining = buf;

        while !remaining.is_empty() {
            // 尝试写入数据
            let write_result = {
                let mut inner = self.inner.lock();

                if inner.read_closed || !has_readers {
                    // 读端关闭，写入失败 (SIGPIPE)
                    Some(Err(FileSystemError::PermissionDenied))
                } else {
                    let available_space = PIPE_BUF_SIZE - inner.buffer.len();
                    if available_space > 0 {
                        // 有空间可写
                        let write_len = remaining.len().min(available_space);
                        for i in 0..write_len {
                            inner.buffer.push_back(remaining[i]);
                        }

                        total_written += write_len;
                        remaining = &remaining[write_len..];

                        // 唤醒等待读取的任务
                        Self::wakeup_waiters(&mut inner.read_wait_queue);

                        Some(Ok(()))
                    } else {
                        // 缓冲区满
                        if total_written > 0 {
                            // 已有部分写入，本次调用到此为止
                            return Ok(total_written);
                        } else {
                            // 尚未写入任何数据，直接返回0，避免单线程死锁
                            return Ok(0);
                        }
                    }
                }
            };

            match write_result {
                Some(Ok(())) => continue, // 继续写入剩余数据
                Some(Err(e)) => return Err(e),
                None => {
                    // 需要阻塞等待
                    block_current_and_run_next();
                    // 任务被唤醒后继续循环检查
                }
            }
        }

        Ok(total_written)
    }

    /// 等待写入器连接（用于FIFO阻塞式打开）
    pub fn wait_for_writer_connection(&self) {
        let need_block = {
            let mut inner = self.inner.lock();
            if let Some(current) = current_task() {
                inner.read_open_wait_queue.push(Arc::downgrade(&current));
                true
            } else {
                false
            }
        };
        if need_block {
            block_current_and_run_next();
        }
    }

    /// 等待读取器连接（用于FIFO阻塞式打开）
    pub fn wait_for_reader_connection(&self) {
        let need_block = {
            let mut inner = self.inner.lock();
            if let Some(current) = current_task() {
                inner.write_open_wait_queue.push(Arc::downgrade(&current));
                true
            } else {
                false
            }
        };
        if need_block {
            block_current_and_run_next();
        }
    }
}

/// 管道读端
pub struct PipeReadEnd {
    pipe: Arc<Pipe>,
}

impl PipeReadEnd {
    pub fn new(pipe: Arc<Pipe>) -> Self {
        Self { pipe }
    }
}

impl Inode for PipeReadEnd {
    fn size(&self) -> u64 {
        0 // 管道没有固定大小
    }

    fn read_at(&self, _offset: u64, buf: &mut [u8]) -> Result<usize, FileSystemError> {
        // 管道读取忽略offset
        self.pipe.read(buf)
    }

    fn write_at(&self, _offset: u64, _buf: &[u8]) -> Result<usize, FileSystemError> {
        // 读端不能写入
        Err(FileSystemError::PermissionDenied)
    }

    fn list_dir(&self) -> Result<Vec<alloc::string::String>, FileSystemError> {
        // 管道不是目录
        Err(FileSystemError::NotDirectory)
    }

    fn find_child(&self, _name: &str) -> Result<Arc<dyn Inode>, FileSystemError> {
        // 管道不支持查找子项
        Err(FileSystemError::NotDirectory)
    }

    fn create_file(&self, _name: &str) -> Result<Arc<dyn Inode>, FileSystemError> {
        Err(FileSystemError::NotDirectory)
    }

    fn create_directory(&self, _name: &str) -> Result<Arc<dyn Inode>, FileSystemError> {
        // 管道不支持创建目录
        Err(FileSystemError::NotDirectory)
    }

    fn remove(&self, _name: &str) -> Result<(), FileSystemError> {
        // 管道不支持移除操作
        Err(FileSystemError::NotDirectory)
    }

    fn inode_type(&self) -> InodeType {
        InodeType::File // 管道作为特殊文件处理
    }

    fn truncate(&self, _size: u64) -> Result<(), FileSystemError> {
        Err(FileSystemError::PermissionDenied)
    }

    fn sync(&self) -> Result<(), FileSystemError> {
        Ok(()) // 管道无需同步
    }
}

impl Drop for PipeReadEnd {
    fn drop(&mut self) {
        self.pipe.close_read();
    }
}

/// 管道写端
pub struct PipeWriteEnd {
    pipe: Arc<Pipe>,
}

impl PipeWriteEnd {
    pub fn new(pipe: Arc<Pipe>) -> Self {
        Self { pipe }
    }
}

impl Inode for PipeWriteEnd {
    fn size(&self) -> u64 {
        0 // 管道没有固定大小
    }

    fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> Result<usize, FileSystemError> {
        // 写端不能读取
        Err(FileSystemError::PermissionDenied)
    }

    fn write_at(&self, _offset: u64, buf: &[u8]) -> Result<usize, FileSystemError> {
        // 管道写入忽略offset
        self.pipe.write(buf)
    }

    fn list_dir(&self) -> Result<Vec<alloc::string::String>, FileSystemError> {
        // 管道不是目录
        Err(FileSystemError::NotDirectory)
    }

    fn find_child(&self, _name: &str) -> Result<Arc<dyn Inode>, FileSystemError> {
        // 管道不支持查找子项
        Err(FileSystemError::NotDirectory)
    }

    fn create_file(&self, _name: &str) -> Result<Arc<dyn Inode>, FileSystemError> {
        Err(FileSystemError::NotDirectory)
    }

    fn create_directory(&self, _name: &str) -> Result<Arc<dyn Inode>, FileSystemError> {
        // 管道不支持创建目录
        Err(FileSystemError::NotDirectory)
    }

    fn remove(&self, _name: &str) -> Result<(), FileSystemError> {
        // 管道不支持移除操作
        Err(FileSystemError::NotDirectory)
    }

    fn inode_type(&self) -> InodeType {
        InodeType::File // 管道作为特殊文件处理
    }

    fn truncate(&self, _size: u64) -> Result<(), FileSystemError> {
        Err(FileSystemError::PermissionDenied)
    }

    fn sync(&self) -> Result<(), FileSystemError> {
        Ok(()) // 管道无需同步
    }
}

impl Drop for PipeWriteEnd {
    fn drop(&mut self) {
        self.pipe.close_write();
    }
}

/// 创建管道对
pub fn create_pipe() -> (Arc<PipeReadEnd>, Arc<PipeWriteEnd>) {
    let pipe = Arc::new(Pipe::new());
    let read_end = Arc::new(PipeReadEnd::new(pipe.clone()));
    let write_end = Arc::new(PipeWriteEnd::new(pipe));
    (read_end, write_end)
}

/// Named Pipe (FIFO) implementation
pub struct NamedPipe {
    pipe: Arc<Pipe>,
    /// Number of read handles currently open
    read_count: atomic::AtomicUsize,
    /// Number of write handles currently open
    write_count: atomic::AtomicUsize,
    /// Permission bits (including type bits as created). 0 means uninitialized
    mode: atomic::AtomicU32,
    /// Owner uid
    owner_uid: atomic::AtomicU32,
    /// Owner gid
    owner_gid: atomic::AtomicU32,
}

impl NamedPipe {
    pub fn new() -> Self {
        Self {
            pipe: Arc::new(Pipe::new()),
            read_count: atomic::AtomicUsize::new(0),
            write_count: atomic::AtomicUsize::new(0),
            mode: atomic::AtomicU32::new(0),
            owner_uid: atomic::AtomicU32::new(0),
            owner_gid: atomic::AtomicU32::new(0),
        }
    }

    /// 是否有读取端连接
    pub fn has_readers(&self) -> bool {
        self.read_count.load(atomic::Ordering::Acquire) > 0
    }

    /// 是否有写入端连接
    pub fn has_writers(&self) -> bool {
        self.write_count.load(atomic::Ordering::Acquire) > 0
    }

    /// Initialize or update metadata (mode/uid/gid). Used by VFS when binding to FS node
    pub fn maybe_init_meta(&self, mode: u32, uid: u32, gid: u32) {
        // Only write once if not initialized
        if self.mode.load(atomic::Ordering::Acquire) == 0 {
            self.mode.store(mode, atomic::Ordering::Release);
            self.owner_uid.store(uid, atomic::Ordering::Release);
            self.owner_gid.store(gid, atomic::Ordering::Release);
        }
    }

    /// Open for reading - blocks until a writer is available if needed
    pub fn open_read(self: &Arc<Self>) -> Arc<FifoReadHandle> {
        self.open_read_with_flags(false)
    }

    /// Open for reading with nonblock option
    pub fn open_read_with_flags(self: &Arc<Self>, nonblock: bool) -> Arc<FifoReadHandle> {
        self.read_count.fetch_add(1, atomic::Ordering::AcqRel);
        // 先通知写端，避免写端因没有读端而阻塞
        self.pipe.notify_reader_connected();
        // 若当前尚无写入端，使用等待队列阻塞等待一次连接
        if !nonblock && self.write_count.load(atomic::Ordering::Acquire) == 0 {
            self.pipe.wait_for_writer_connection();
        }
        Arc::new(FifoReadHandle::new(self.pipe.clone(), self.clone()))
    }

    /// Open for writing - blocks until a reader is available if needed
    pub fn open_write(self: &Arc<Self>) -> Arc<FifoWriteHandle> {
        self.open_write_with_flags(false)
    }

    /// Open for writing with nonblock option
    pub fn open_write_with_flags(self: &Arc<Self>, nonblock: bool) -> Arc<FifoWriteHandle> {
        self.write_count.fetch_add(1, atomic::Ordering::AcqRel);
        // 先通知读端，避免读端因没有写端而阻塞
        self.pipe.notify_writer_connected();
        // 若当前尚无读端，使用等待队列阻塞等待一次连接
        if !nonblock && self.read_count.load(atomic::Ordering::Acquire) == 0 {
            self.pipe.wait_for_reader_connection();
        }
        Arc::new(FifoWriteHandle::new(self.pipe.clone(), self.clone()))
    }

    fn close_reader(&self) {
        let prev_count = self.read_count.fetch_sub(1, atomic::Ordering::Acquire);
        if prev_count == 1 {
            // 当前是最后一个读取句柄
            self.pipe.close_read();
        }
    }

    fn close_writer(&self) {
        let prev_count = self.write_count.fetch_sub(1, atomic::Ordering::Acquire);
        if prev_count == 1 {
            // 当前是最后一个写入句柄
            self.pipe.close_write();
        }
    }
}

impl Inode for NamedPipe {
    fn inode_type(&self) -> InodeType {
        InodeType::Fifo
    }

    fn size(&self) -> u64 {
        0 // FIFOs don't have a fixed size
    }

    fn read_at(&self, _offset: u64, buf: &mut [u8]) -> Result<usize, FileSystemError> {
        // For FIFO, reading through the inode interface uses the pipe directly
        self.pipe.read(buf)
    }

    fn write_at(&self, _offset: u64, buf: &[u8]) -> Result<usize, FileSystemError> {
        // For FIFO, writing through the inode interface uses the pipe directly
        self.pipe.write(buf)
    }

    fn list_dir(&self) -> Result<Vec<String>, FileSystemError> {
        Err(FileSystemError::NotDirectory)
    }

    fn find_child(&self, _name: &str) -> Result<Arc<dyn Inode>, FileSystemError> {
        Err(FileSystemError::NotDirectory)
    }

    fn create_file(&self, _name: &str) -> Result<Arc<dyn Inode>, FileSystemError> {
        Err(FileSystemError::NotDirectory)
    }

    fn create_directory(&self, _name: &str) -> Result<Arc<dyn Inode>, FileSystemError> {
        Err(FileSystemError::NotDirectory)
    }

    fn remove(&self, _name: &str) -> Result<(), FileSystemError> {
        Err(FileSystemError::NotDirectory)
    }

    fn truncate(&self, _size: u64) -> Result<(), FileSystemError> {
        Err(FileSystemError::PermissionDenied)
    }

    fn sync(&self) -> Result<(), FileSystemError> {
        Ok(())
    }

    fn mode(&self) -> u32 {
        self.mode.load(atomic::Ordering::Acquire)
    }
    fn uid(&self) -> u32 {
        self.owner_uid.load(atomic::Ordering::Acquire)
    }
    fn gid(&self) -> u32 {
        self.owner_gid.load(atomic::Ordering::Acquire)
    }

    fn poll_mask(&self) -> u32 {
        const POLLIN: u32 = 0x0001;
        const POLLOUT: u32 = 0x0004;
        const POLLHUP: u32 = 0x0010;
        let inner = self.pipe.inner.lock();
        let mut mask = 0u32;
        if !inner.buffer.is_empty() {
            mask |= POLLIN;
        }
        // 对于命名管道，若存在读端则可写；这里以 read_closed 取代计数简化
        if inner.buffer.len() < PIPE_BUF_SIZE && !inner.read_closed {
            mask |= POLLOUT;
        }
        // 若无读端或无写端，报告 HUP（更完整可区分 R/W 端）
        if inner.read_closed || inner.write_closed {
            mask |= POLLHUP;
        }
        mask
    }

    fn register_poll_waiter(&self, interests: u32, task: Arc<crate::task::TaskControlBlock>) {
        let mut inner = self.pipe.inner.lock();
        inner
            .poll_waiters
            .insert(task.pid(), (Arc::downgrade(&task), interests));
    }

    fn clear_poll_waiter(&self, task_pid: usize) {
        let mut inner = self.pipe.inner.lock();
        inner.poll_waiters.remove(&task_pid);
    }
}

/// FIFO read handle
pub struct FifoReadHandle {
    pipe: Arc<Pipe>,
    fifo: Arc<NamedPipe>,
}

impl FifoReadHandle {
    fn new(pipe: Arc<Pipe>, fifo: Arc<NamedPipe>) -> Self {
        Self { pipe, fifo }
    }
}

impl Inode for FifoReadHandle {
    fn inode_type(&self) -> InodeType {
        InodeType::Fifo
    }

    fn size(&self) -> u64 {
        0
    }

    fn read_at(&self, _offset: u64, buf: &mut [u8]) -> Result<usize, FileSystemError> {
        // 检查是否有写入器连接
        let has_writers = self.fifo.write_count.load(atomic::Ordering::Acquire) > 0;
        self.pipe.fifo_read(buf, has_writers)
    }

    fn write_at(&self, _offset: u64, _buf: &[u8]) -> Result<usize, FileSystemError> {
        Err(FileSystemError::PermissionDenied)
    }

    fn list_dir(&self) -> Result<Vec<String>, FileSystemError> {
        Err(FileSystemError::NotDirectory)
    }

    fn find_child(&self, _name: &str) -> Result<Arc<dyn Inode>, FileSystemError> {
        Err(FileSystemError::NotDirectory)
    }

    fn create_file(&self, _name: &str) -> Result<Arc<dyn Inode>, FileSystemError> {
        Err(FileSystemError::NotDirectory)
    }

    fn create_directory(&self, _name: &str) -> Result<Arc<dyn Inode>, FileSystemError> {
        Err(FileSystemError::NotDirectory)
    }

    fn remove(&self, _name: &str) -> Result<(), FileSystemError> {
        Err(FileSystemError::NotDirectory)
    }

    fn truncate(&self, _size: u64) -> Result<(), FileSystemError> {
        Err(FileSystemError::PermissionDenied)
    }

    fn sync(&self) -> Result<(), FileSystemError> {
        Ok(())
    }
    fn mode(&self) -> u32 {
        self.fifo.mode.load(atomic::Ordering::Acquire)
    }
    fn uid(&self) -> u32 {
        self.fifo.owner_uid.load(atomic::Ordering::Acquire)
    }
    fn gid(&self) -> u32 {
        self.fifo.owner_gid.load(atomic::Ordering::Acquire)
    }
}

impl Drop for FifoReadHandle {
    fn drop(&mut self) {
        self.fifo.close_reader();
    }
}

/// FIFO write handle
pub struct FifoWriteHandle {
    pipe: Arc<Pipe>,
    fifo: Arc<NamedPipe>,
}

impl FifoWriteHandle {
    fn new(pipe: Arc<Pipe>, fifo: Arc<NamedPipe>) -> Self {
        Self { pipe, fifo }
    }
}

impl Inode for FifoWriteHandle {
    fn inode_type(&self) -> InodeType {
        InodeType::Fifo
    }

    fn size(&self) -> u64 {
        0
    }

    fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> Result<usize, FileSystemError> {
        Err(FileSystemError::PermissionDenied)
    }

    fn write_at(&self, _offset: u64, buf: &[u8]) -> Result<usize, FileSystemError> {
        // 检查是否有读取器连接
        let has_readers = self.fifo.read_count.load(atomic::Ordering::Acquire) > 0;
        self.pipe.fifo_write(buf, has_readers)
    }

    fn list_dir(&self) -> Result<Vec<String>, FileSystemError> {
        Err(FileSystemError::NotDirectory)
    }

    fn find_child(&self, _name: &str) -> Result<Arc<dyn Inode>, FileSystemError> {
        Err(FileSystemError::NotDirectory)
    }

    fn create_file(&self, _name: &str) -> Result<Arc<dyn Inode>, FileSystemError> {
        Err(FileSystemError::NotDirectory)
    }

    fn create_directory(&self, _name: &str) -> Result<Arc<dyn Inode>, FileSystemError> {
        Err(FileSystemError::NotDirectory)
    }

    fn remove(&self, _name: &str) -> Result<(), FileSystemError> {
        Err(FileSystemError::NotDirectory)
    }

    fn truncate(&self, _size: u64) -> Result<(), FileSystemError> {
        Err(FileSystemError::PermissionDenied)
    }

    fn sync(&self) -> Result<(), FileSystemError> {
        Ok(())
    }
    fn mode(&self) -> u32 {
        self.fifo.mode.load(atomic::Ordering::Acquire)
    }
    fn uid(&self) -> u32 {
        self.fifo.owner_uid.load(atomic::Ordering::Acquire)
    }
    fn gid(&self) -> u32 {
        self.fifo.owner_gid.load(atomic::Ordering::Acquire)
    }
}

impl Drop for FifoWriteHandle {
    fn drop(&mut self) {
        self.fifo.close_writer();
    }
}

/// Global FIFO registry to manage named pipes by path
static FIFO_REGISTRY: Mutex<BTreeMap<String, Arc<NamedPipe>>> = Mutex::new(BTreeMap::new());

/// Create a new named pipe (FIFO) at the given path
pub fn create_fifo(path: &str) -> Result<Arc<NamedPipe>, FileSystemError> {
    let mut registry = FIFO_REGISTRY.lock();

    if registry.contains_key(path) {
        return Err(FileSystemError::AlreadyExists);
    }

    let fifo = Arc::new(NamedPipe::new());
    registry.insert(path.to_string(), fifo.clone());
    Ok(fifo)
}

/// Open an existing named pipe
pub fn open_fifo(path: &str) -> Result<Arc<NamedPipe>, FileSystemError> {
    let registry = FIFO_REGISTRY.lock();
    registry
        .get(path)
        .map(|fifo| fifo.clone())
        .ok_or(FileSystemError::NotFound)
}

/// Remove a named pipe from the registry
pub fn remove_fifo(path: &str) -> Result<(), FileSystemError> {
    let mut registry = FIFO_REGISTRY.lock();
    registry
        .remove(path)
        .map(|_| ())
        .ok_or(FileSystemError::NotFound)
}
