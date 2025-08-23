use alloc::{string::String, sync::Arc, vec::Vec};

#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InodeType {
    File = 0,
    Directory = 1,
    SymLink = 2,
    Device = 3,
    Fifo = 4, // Named pipe (FIFO)
}

pub trait Inode: Send + Sync {
    fn inode_type(&self) -> InodeType;
    fn size(&self) -> u64;
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, super::FileSystemError>;
    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<usize, super::FileSystemError>;
    fn list_dir(&self) -> Result<Vec<String>, super::FileSystemError>;
    fn find_child(&self, name: &str) -> Result<Arc<dyn Inode>, super::FileSystemError>;
    fn create_file(&self, name: &str) -> Result<Arc<dyn Inode>, super::FileSystemError>;
    fn create_directory(&self, name: &str) -> Result<Arc<dyn Inode>, super::FileSystemError>;
    fn remove(&self, name: &str) -> Result<(), super::FileSystemError>;
    fn truncate(&self, size: u64) -> Result<(), super::FileSystemError>;
    fn sync(&self) -> Result<(), super::FileSystemError>;

    /// 获取文件权限模式（默认实现为0o644）
    fn mode(&self) -> u32 {
        0o644
    }

    /// 设置文件权限模式（默认实现不做任何操作）
    fn set_mode(&self, _mode: u32) -> Result<(), super::FileSystemError> {
        Ok(())
    }

    /// 获取文件拥有者UID（默认实现为0，即root）
    fn uid(&self) -> u32 {
        0
    }

    /// 设置文件拥有者UID（默认实现不做任何操作）
    fn set_uid(&self, _uid: u32) -> Result<(), super::FileSystemError> {
        Ok(())
    }

    /// 获取文件拥有者GID（默认实现为0，即root组）
    fn gid(&self) -> u32 {
        0
    }

    /// 设置文件拥有者GID（默认实现不做任何操作）
    fn set_gid(&self, _gid: u32) -> Result<(), super::FileSystemError> {
        Ok(())
    }

    /// 事件就绪掩码（用于 poll/select）。默认认为可读可写。
    /// 位定义与内核 sys_poll 常量一致：POLLIN=0x0001, POLLOUT=0x0004
    fn poll_mask(&self) -> u32 {
        0x0001 | 0x0004
    }

    /// 注册 poll 等待者（默认空实现）
    fn register_poll_waiter(
        &self,
        _interests: u32,
        _task: alloc::sync::Arc<crate::task::TaskControlBlock>,
    ) {
    }

    fn atime(&self) -> u64 { 0 }
    fn mtime(&self) -> u64 { 0 }
    fn ctime(&self) -> u64 { 0 }

    /// 取消注册 poll 等待者（默认空实现）
    fn clear_poll_waiter(&self, _task_pid: usize) {}
}
