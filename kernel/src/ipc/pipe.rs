use alloc::{sync::{Arc, Weak}, vec::Vec, collections::VecDeque};
use crate::{
    sync::UPSafeCell,
    fs::{FileSystemError, inode::{Inode, InodeType}},
    task::{TaskControlBlock, current_task, block_current_and_run_next, wakeup_task},
};

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
        let buffer = self.buffer.exclusive_access();
        !buffer.is_empty() || *self.write_closed.exclusive_access()
    }

    /// 检查是否可以写入
    pub fn can_write(&self) -> bool {
        let buffer = self.buffer.exclusive_access();
        buffer.len() < PIPE_BUF_SIZE && !*self.read_closed.exclusive_access()
    }

    /// 从管道读取数据（阻塞式）
    pub fn read(&self, buf: &mut [u8]) -> Result<usize, FileSystemError> {
        loop {
            let mut buffer = self.buffer.exclusive_access();
            let write_closed = *self.write_closed.exclusive_access();
            
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
            let mut buffer = self.buffer.exclusive_access();
            let read_closed = *self.read_closed.exclusive_access();
            
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