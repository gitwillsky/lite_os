//! 文件页缓存向内存映射提供的接口。

use alloc::sync::Arc;
use core::fmt::Debug;

use super::MemoryObject;

/// 映射期间需要保活的打开文件或设备对象（mm 只持有引用，不解释其内容）。
pub(crate) trait MappedFileOwner: core::any::Any + Send + Sync + Debug {}

/// 一个缓存文件的全局身份（文件系统实例 + inode）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct SharedFileId {
    pub(crate) filesystem: usize,
    pub(crate) inode: u64,
}

/// 页缓存访问失败。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SharedFileError {
    OutOfMemory,
    Io,
    BeyondEof,
}

/// 文件映射的后备：页缓存 VMO 是文件内容的唯一驻留副本，read/write 与 mmap 看到同一页。
pub(crate) trait SharedFileMapping: Send + Sync + Debug {
    /// 打开方式允许共享写映射（`VM_MAYWRITE`）。
    fn may_write(&self) -> bool {
        true
    }
    /// 页缓存对象。
    ///
    /// # Errors
    ///
    /// 缓存对象创建失败。
    fn object(&self) -> Result<Arc<MemoryObject>, SharedFileError>;
    /// 把 `[offset, offset + length)` 的脏页写回存储（`msync(MS_SYNC)`）。
    ///
    /// # Errors
    ///
    /// 写回 I/O 失败。
    fn sync_range(&self, offset: u64, length: u64) -> Result<(), SharedFileError>;
}
