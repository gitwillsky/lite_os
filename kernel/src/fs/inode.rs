use alloc::sync::Arc;

#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InodeType {
    File = 0,
    Directory = 1,
    SymLink = 2,
    Fifo = 4, // Named pipe (FIFO)
}

pub trait Inode: Send + Sync {
    /// 返回 inode 在磁盘上记录的类型。
    fn inode_type(&self) -> InodeType;

    /// 返回文件的字节长度。
    fn size(&self) -> u64;

    /// @description 按磁盘 mode 判断 root identity 是否可执行该 inode。
    ///
    /// @return 普通文件任一 execute bit 置位时返回 `true`。
    fn is_executable(&self) -> bool;

    /// 从指定偏移读取 inode 数据。
    ///
    /// # Parameters
    ///
    /// - `offset`: 起始字节偏移。
    /// - `buf`: 接收数据的内核缓冲区。
    ///
    /// # Returns
    ///
    /// 成功时返回实际读取字节数；到达 EOF 返回 `0`。
    ///
    /// # Errors
    ///
    /// 块设备读取或 inode 映射失败时返回对应的 `FileSystemError`。
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, super::FileSystemError>;

    /// 在目录 inode 中查找直接子项。
    ///
    /// # Parameters
    ///
    /// - `name`: 不含路径分隔符的单个目录项名。
    ///
    /// # Returns
    ///
    /// 成功时返回子 inode 的共享引用。
    ///
    /// # Errors
    ///
    /// 当前 inode 不是目录、子项不存在或磁盘数据无效时返回错误。
    fn find_child(&self, name: &[u8]) -> Result<Arc<dyn Inode>, super::FileSystemError>;
}
