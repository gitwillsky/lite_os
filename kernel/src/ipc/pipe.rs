use core::sync::atomic;

use alloc::{sync::{Arc, Weak}, vec::Vec, collections::{VecDeque, BTreeMap}, string::{String, ToString}};
use crate::{
    fs::{FileSystemError, inode::{Inode, InodeType}},
    task::{TaskControlBlock, current_task, block_current_and_run_next },
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
}

impl NamedPipe {
    pub fn new() -> Self {
        Self {
            pipe: Arc::new(Pipe::new()),
            read_count: atomic::AtomicUsize::new(0),
            write_count: atomic::AtomicUsize::new(0),
        }
    }

    /// Open for reading - blocks until a writer is available if needed
    pub fn open_read(self: &Arc<Self>) -> Arc<FifoReadHandle> {
        self.read_count.fetch_add(1, atomic::Ordering::AcqRel);
        // 先通知写端，避免写端因没有读端而阻塞
        self.pipe.notify_reader_connected();
        // 若当前尚无写入端，使用等待队列阻塞等待一次连接
        if self.write_count.load(atomic::Ordering::Acquire) == 0 {
            self.pipe.wait_for_writer_connection();
        }
        Arc::new(FifoReadHandle::new(self.pipe.clone(), Arc::downgrade(self)))
    }

    /// Open for writing - blocks until a reader is available if needed
    pub fn open_write(self: &Arc<Self>) -> Arc<FifoWriteHandle> {
        self.write_count.fetch_add(1, atomic::Ordering::AcqRel);
        // 先通知读端，避免读端因没有写端而阻塞
        self.pipe.notify_writer_connected();
        // 若当前尚无读端，使用等待队列阻塞等待一次连接
        if self.read_count.load(atomic::Ordering::Acquire) == 0 {
            self.pipe.wait_for_reader_connection();
        }
        Arc::new(FifoWriteHandle::new(self.pipe.clone(), Arc::downgrade(self)))
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
}

/// FIFO read handle
pub struct FifoReadHandle {
    pipe: Arc<Pipe>,
    fifo: Weak<NamedPipe>,
}

impl FifoReadHandle {
    fn new(pipe: Arc<Pipe>, fifo: Weak<NamedPipe>) -> Self {
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
        let has_writers = if let Some(fifo) = self.fifo.upgrade() {
            fifo.write_count.load(atomic::Ordering::Acquire) > 0
        } else {
            false
        };
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
}

impl Drop for FifoReadHandle {
    fn drop(&mut self) {
        if let Some(fifo) = self.fifo.upgrade() {
            fifo.close_reader();
        }
    }
}

/// FIFO write handle
pub struct FifoWriteHandle {
    pipe: Arc<Pipe>,
    fifo: Weak<NamedPipe>,
}

impl FifoWriteHandle {
    fn new(pipe: Arc<Pipe>, fifo: Weak<NamedPipe>) -> Self {
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
        let has_readers = if let Some(fifo) = self.fifo.upgrade() {
            fifo.read_count.load(atomic::Ordering::Acquire) > 0
        } else {
            false
        };
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
}

impl Drop for FifoWriteHandle {
    fn drop(&mut self) {
        if let Some(fifo) = self.fifo.upgrade() {
            fifo.close_writer();
        }
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
    registry.get(path)
        .map(|fifo| fifo.clone())
        .ok_or(FileSystemError::NotFound)
}

/// Remove a named pipe from the registry
pub fn remove_fifo(path: &str) -> Result<(), FileSystemError> {
    let mut registry = FIFO_REGISTRY.lock();
    registry.remove(path)
        .map(|_| ())
        .ok_or(FileSystemError::NotFound)
}