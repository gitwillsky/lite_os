use alloc::{sync::{Arc, Weak}, vec::Vec, collections::{VecDeque, BTreeMap}, string::{String, ToString}};
use crate::{
    sync::UPSafeCell,
    fs::{FileSystemError, inode::{Inode, InodeType}},
    task::{TaskControlBlock, current_task, block_current_and_run_next, wakeup_task},
};
use spin::Mutex;

/// 管道缓冲区大小
const PIPE_BUF_SIZE: usize = 4096;

/// 管道结构体
pub struct Pipe {
    /// 数据缓冲区
    buffer: UPSafeCell<VecDeque<u8>>,
    /// 读端是否关闭
    read_closed: UPSafeCell<bool>,
    /// 写端是否关闭
    write_closed: UPSafeCell<bool>,
    /// 等待读取的任务队列
    read_wait_queue: UPSafeCell<Vec<Weak<TaskControlBlock>>>,
    /// 等待写入的任务队列
    write_wait_queue: UPSafeCell<Vec<Weak<TaskControlBlock>>>,
}

impl Pipe {
    /// 创建新的管道
    pub fn new() -> Self {
        Self {
            buffer: UPSafeCell::new(VecDeque::with_capacity(PIPE_BUF_SIZE)),
            read_closed: UPSafeCell::new(false),
            write_closed: UPSafeCell::new(false),
            read_wait_queue: UPSafeCell::new(Vec::new()),
            write_wait_queue: UPSafeCell::new(Vec::new()),
        }
    }

    /// 关闭读端
    pub fn close_read(&self) {
        *self.read_closed.exclusive_access() = true;
        // 唤醒所有等待写入的任务
        self.wakeup_write_waiters();
    }

    /// 关闭写端
    pub fn close_write(&self) {
        *self.write_closed.exclusive_access() = true;
        // 唤醒所有等待读取的任务
        self.wakeup_read_waiters();
    }

    /// 将当前任务添加到读等待队列
    fn add_read_waiter(&self, task: Weak<TaskControlBlock>) {
        self.read_wait_queue.exclusive_access().push(task);
    }

    /// 将当前任务添加到写等待队列
    fn add_write_waiter(&self, task: Weak<TaskControlBlock>) {
        self.write_wait_queue.exclusive_access().push(task);
    }

    /// 唤醒所有等待读取的任务
    fn wakeup_read_waiters(&self) {
        let mut waiters = self.read_wait_queue.exclusive_access();
        waiters.retain(|weak_task| {
            if let Some(task) = weak_task.upgrade() {
                wakeup_task(task);
                false // 从等待队列中移除
            } else {
                false // 任务已经被回收，从队列中移除
            }
        });
    }

    /// 唤醒所有等待写入的任务
    fn wakeup_write_waiters(&self) {
        let mut waiters = self.write_wait_queue.exclusive_access();
        waiters.retain(|weak_task| {
            if let Some(task) = weak_task.upgrade() {
                wakeup_task(task);
                false // 从等待队列中移除
            } else {
                false // 任务已经被回收，从队列中移除
            }
        });
    }

    /// 检查是否可以读取
    pub fn can_read(&self) -> bool {
        let write_closed = *self.write_closed.exclusive_access();
        let buffer = self.buffer.exclusive_access();
        !buffer.is_empty() || write_closed
    }

    /// 检查是否可以写入
    pub fn can_write(&self) -> bool {
        let read_closed = *self.read_closed.exclusive_access();
        let buffer = self.buffer.exclusive_access();
        buffer.len() < PIPE_BUF_SIZE && !read_closed
    }

    /// 从管道读取数据（阻塞式）
    pub fn read(&self, buf: &mut [u8]) -> Result<usize, FileSystemError> {
        loop {
            // 先检查写端状态，避免借用冲突
            let write_closed = *self.write_closed.exclusive_access();
            let mut buffer = self.buffer.exclusive_access();

            if !buffer.is_empty() {
                // 有数据可读，立即返回
                let read_len = buf.len().min(buffer.len());
                for i in 0..read_len {
                    buf[i] = buffer.pop_front().unwrap();
                }

                // 唤醒等待写入的任务（缓冲区有空间了）
                drop(buffer);
                self.wakeup_write_waiters();

                return Ok(read_len);
            } else if write_closed {
                // 写端关闭且无数据，返回EOF
                return Ok(0);
            } else {
                // 无数据且写端未关闭，需要阻塞等待
                drop(buffer);

                if let Some(current) = current_task() {
                    // 将当前任务添加到等待队列
                    self.add_read_waiter(Arc::downgrade(&current));

                    // 阻塞当前任务
                    block_current_and_run_next();

                    // 任务被唤醒后继续循环检查
                } else {
                    return Err(FileSystemError::IoError);
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
            // 先检查读端状态，避免借用冲突
            let read_closed = *self.read_closed.exclusive_access();
            let mut buffer = self.buffer.exclusive_access();

            if read_closed {
                // 读端关闭，写入失败 (SIGPIPE)
                return Err(FileSystemError::PermissionDenied);
            }

            let available_space = PIPE_BUF_SIZE - buffer.len();
            if available_space > 0 {
                // 有空间可写
                let write_len = remaining.len().min(available_space);
                for i in 0..write_len {
                    buffer.push_back(remaining[i]);
                }

                total_written += write_len;
                remaining = &remaining[write_len..];

                // 唤醒等待读取的任务（有新数据了）
                drop(buffer);
                self.wakeup_read_waiters();

                // 如果还有数据要写但缓冲区满了，继续循环阻塞
            } else {
                // 缓冲区满，需要阻塞等待
                drop(buffer);

                if let Some(current) = current_task() {
                    // 将当前任务添加到等待队列
                    self.add_write_waiter(Arc::downgrade(&current));

                    // 阻塞当前任务
                    block_current_and_run_next();

                    // 任务被唤醒后继续循环检查
                } else {
                    break; // 无法获取当前任务，退出循环
                }
            }
        }

        Ok(total_written)
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
    read_count: UPSafeCell<usize>,
    /// Number of write handles currently open
    write_count: UPSafeCell<usize>,
}

impl NamedPipe {
    pub fn new() -> Self {
        Self {
            pipe: Arc::new(Pipe::new()),
            read_count: UPSafeCell::new(0),
            write_count: UPSafeCell::new(0),
        }
    }

    /// Open for reading - blocks until a writer is available if needed
    pub fn open_read(self: &Arc<Self>) -> Arc<FifoReadHandle> {
        *self.read_count.exclusive_access() += 1;
        Arc::new(FifoReadHandle::new(self.pipe.clone(), Arc::downgrade(self)))
    }

    /// Open for writing - blocks until a reader is available if needed
    pub fn open_write(self: &Arc<Self>) -> Arc<FifoWriteHandle> {
        *self.write_count.exclusive_access() += 1;
        Arc::new(FifoWriteHandle::new(self.pipe.clone(), Arc::downgrade(self)))
    }

    fn close_reader(&self) {
        let mut count = self.read_count.exclusive_access();
        if *count > 0 {
            *count -= 1;
            if *count == 0 {
                self.pipe.close_read();
            }
        }
    }

    fn close_writer(&self) {
        let mut count = self.write_count.exclusive_access();
        if *count > 0 {
            *count -= 1;
            if *count == 0 {
                self.pipe.close_write();
            }
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
        self.pipe.read(buf)
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