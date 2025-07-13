use alloc::{sync::Arc, vec::Vec, collections::VecDeque};
use crate::{
    sync::UPSafeCell,
    fs::{FileSystemError, inode::{Inode, InodeType}},
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
}

impl Pipe {
    /// 创建新的管道
    pub fn new() -> Self {
        Self {
            buffer: UPSafeCell::new(VecDeque::with_capacity(PIPE_BUF_SIZE)),
            read_closed: UPSafeCell::new(false),
            write_closed: UPSafeCell::new(false),
        }
    }

    /// 关闭读端
    pub fn close_read(&self) {
        *self.read_closed.exclusive_access() = true;
    }

    /// 关闭写端
    pub fn close_write(&self) {
        *self.write_closed.exclusive_access() = true;
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

    /// 从管道读取数据
    pub fn read(&self, buf: &mut [u8]) -> Result<usize, FileSystemError> {
        let mut buffer = self.buffer.exclusive_access();
        let write_closed = *self.write_closed.exclusive_access();
        
        if buffer.is_empty() {
            if write_closed {
                // 写端关闭且无数据，返回EOF
                return Ok(0);
            } else {
                // 无数据且写端未关闭，应该阻塞，这里返回错误表示需要重试
                return Err(FileSystemError::IoError);
            }
        }

        let read_len = buf.len().min(buffer.len());
        for i in 0..read_len {
            buf[i] = buffer.pop_front().unwrap();
        }
        
        Ok(read_len)
    }

    /// 向管道写入数据
    pub fn write(&self, buf: &[u8]) -> Result<usize, FileSystemError> {
        let mut buffer = self.buffer.exclusive_access();
        let read_closed = *self.read_closed.exclusive_access();
        
        if read_closed {
            // 读端关闭，写入失败 (SIGPIPE)
            return Err(FileSystemError::IoError);
        }

        let available_space = PIPE_BUF_SIZE - buffer.len();
        if available_space == 0 {
            // 缓冲区满，应该阻塞，这里返回错误表示需要重试
            return Err(FileSystemError::IoError);
        }

        let write_len = buf.len().min(available_space);
        for i in 0..write_len {
            buffer.push_back(buf[i]);
        }
        
        Ok(write_len)
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