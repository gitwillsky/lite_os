use alloc::{sync::Arc, vec::Vec};

pub(crate) use crate::{FileSystemError, InodeType};

#[derive(Debug, Clone, Copy)]
pub(crate) struct CreateMetadata {
    pub(crate) mode: u32,
    pub(crate) uid: u32,
    pub(crate) gid: u32,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct DirectoryEntry<'a> {
    pub(crate) inode: u64,
    pub(crate) kind: InodeType,
    pub(crate) name: &'a [u8],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DirectoryVisit {
    Continue,
    Stop,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct DirectoryRead {
    pub(crate) cursor: u64,
    pub(crate) eof: bool,
}

pub(crate) trait DirectoryVisitor {
    fn visit(
        &mut self,
        next_cursor: u64,
        entry: DirectoryEntry<'_>,
    ) -> Result<DirectoryVisit, FileSystemError>;
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct FineTimes {
    pub(crate) atime_nsec: u32,
    pub(crate) mtime_nsec: u32,
    pub(crate) ctime_nsec: u32,
    pub(crate) birth: Option<(u64, u32)>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct InodeMetadata {
    pub(crate) filesystem: u64,
    pub(crate) inode: u64,
    pub(crate) kind: InodeType,
    pub(crate) mode: u32,
    pub(crate) links: u32,
    pub(crate) uid: u32,
    pub(crate) gid: u32,
    pub(crate) size: u64,
    pub(crate) blocks: u64,
    pub(crate) block_size: u32,
    pub(crate) atime: u64,
    pub(crate) mtime: u64,
    pub(crate) ctime: u64,
    pub(crate) fine_times: FineTimes,
    pub(crate) device: Option<DeviceNumber>,
}

// 加载实际 dev_t codec；块设备注册表是 host storage adapter，不复制编号算法。
#[path = "../../../kernel/src/fs/devnum.rs"]
mod device_number;
pub(crate) use device_number::DeviceNumber;

pub(crate) use crate::block::CompletionNotify;

/// 与 `kernel/src/fs/inode.rs` 的 `StorageStep`/`StorageRead` 同形；ext4 的步进读实现它们。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StorageStep {
    Done(usize),
    Wait,
    Fallback,
}

pub(crate) trait StorageRead: Send {
    fn step(&mut self, output: &mut [u8]) -> Result<StorageStep, FileSystemError>;
}

pub(crate) trait StorageWriter {
    fn write(&mut self, offset: u64, bytes: &[u8]) -> Result<usize, FileSystemError>;
}

pub(crate) trait Inode: Send + Sync {
    fn notification_link_state(&self) -> Option<(u64, u32)> {
        self.metadata()
            .ok()
            .map(|metadata| (metadata.inode, metadata.links))
    }

    fn check_xattr_write(&self) -> Result<(), FileSystemError> {
        Ok(())
    }
    fn read_xattr(&self, _name: &[u8]) -> Result<Vec<u8>, FileSystemError> {
        Err(FileSystemError::NotSupported)
    }
    fn list_xattrs(&self, _identity: &AccessIdentity) -> Result<Vec<u8>, FileSystemError> {
        Err(FileSystemError::NotSupported)
    }
    fn change_xattr(&self, _change: XattrChange<'_>) -> Result<(), FileSystemError> {
        Err(FileSystemError::NotSupported)
    }
    fn write_access(&self) -> Option<&InodeWriteAccess> {
        None
    }
    fn touch_access(&self, _now: u64, _relatime: bool) -> Result<(), FileSystemError> {
        Ok(())
    }
    fn filesystem_id(&self) -> usize;
    fn metadata(&self) -> Result<InodeMetadata, FileSystemError>;
    fn inode_type(&self) -> InodeType;
    fn size(&self) -> u64;
    fn is_swap_file(&self) -> bool {
        false
    }
    fn pin_swap_file(
        self: Arc<Self>,
    ) -> Result<alloc::boxed::Box<dyn swap::SwapFileLease>, FileSystemError> {
        Err(FileSystemError::InvalidOperation)
    }
    fn is_executable(&self) -> bool;
    fn device_number(&self) -> Option<DeviceNumber>;
    fn mknod(
        &self,
        name: &[u8],
        kind: InodeType,
        metadata: CreateMetadata,
        device: Option<DeviceNumber>,
    ) -> Result<Arc<dyn Inode>, FileSystemError>;
    fn read_storage(&self, offset: u64, bytes: &mut [u8]) -> Result<usize, FileSystemError>;
    fn begin_storage_read(
        &self,
        _offset: u64,
        _length: usize,
        _notify: Arc<dyn CompletionNotify>,
    ) -> Result<Option<alloc::boxed::Box<dyn StorageRead>>, FileSystemError> {
        Ok(None)
    }
    fn read_link(&self) -> Result<Vec<u8>, FileSystemError>;
    fn write_storage(
        &self,
        offset: u64,
        bytes: &[u8],
        identity: Option<&crate::fs::AccessIdentity>,
    ) -> Result<usize, FileSystemError>;
    /// 非零 regular write 检查完成后、user-copy 前的 killpriv commit；实际内容提交仍复查 live mode。
    fn prepare_cached_write(
        &self,
        _identity: &crate::fs::AccessIdentity,
    ) -> Result<(), FileSystemError> {
        if self.inode_type() == crate::fs::InodeType::File {
            Err(FileSystemError::ReadOnly)
        } else {
            Ok(())
        }
    }

    fn writeback_storage_batch(
        &self,
        batch: &mut dyn FnMut(&mut dyn StorageWriter) -> Result<(), FileSystemError>,
    ) -> Result<(), FileSystemError>;
    fn try_writeback_storage_batch(
        &self,
        batch: &mut dyn FnMut(&mut dyn StorageWriter) -> Result<(), FileSystemError>,
    ) -> Result<(), FileSystemError>;
    fn prepare_memory_write(
        &self,
        _writer: &memory_file::MemoryWrite<'_>,
        _identity: Option<&AccessIdentity>,
    ) -> Result<(), FileSystemError> {
        Err(FileSystemError::InvalidOperation)
    }
    fn append_storage(
        &self,
        bytes: &[u8],
        identity: Option<&crate::fs::AccessIdentity>,
    ) -> Result<(u64, usize), FileSystemError>;
    fn truncate_storage(
        &self,
        size: u64,
        identity: Option<&crate::fs::AccessIdentity>,
    ) -> Result<(), FileSystemError>;
    fn allocate_storage(
        &self,
        offset: u64,
        length: u64,
        identity: Option<&crate::fs::AccessIdentity>,
    ) -> Result<(), FileSystemError>;
    fn sync_storage(&self) -> Result<(), FileSystemError>;
    fn set_times(&self, atime: Option<u64>, mtime: Option<u64>) -> Result<(), FileSystemError>;
    fn read_directory(
        &self,
        cursor: u64,
        visitor: &mut dyn DirectoryVisitor,
    ) -> Result<DirectoryRead, FileSystemError>;
    fn find_child(&self, name: &[u8]) -> Result<Arc<dyn Inode>, FileSystemError>;
    fn create(
        &self,
        name: &[u8],
        kind: InodeType,
        metadata: CreateMetadata,
    ) -> Result<Arc<dyn Inode>, FileSystemError>;
    fn change_owner_mode(&self, change: OwnerModeChange) -> Result<(), FileSystemError>;
    fn symlink(
        &self,
        name: &[u8],
        target: &[u8],
        metadata: CreateMetadata,
    ) -> Result<Arc<dyn Inode>, FileSystemError>;
    fn link(&self, name: &[u8], target: Arc<dyn Inode>) -> Result<(), FileSystemError>;
    fn unlink(&self, name: &[u8], remove_directory: bool) -> Result<(), FileSystemError>;
    fn rename(
        &self,
        old_name: &[u8],
        new_parent_inode: u64,
        new_name: &[u8],
        no_replace: bool,
    ) -> Result<(), FileSystemError>;
}

pub(crate) trait FileSystem: Send + Sync {
    fn show_mount_options(&self) -> Result<Vec<u8>, FileSystemError> {
        Ok(Vec::new())
    }
    fn superblock(&self) -> SuperblockFacts;
    fn root_inode(&self) -> Result<Arc<dyn Inode>, FileSystemError>;
    fn statistics(&self) -> Result<FileSystemStatistics, FileSystemError>;
    fn remount(
        &self,
        options: &[u8],
        _namespace: &crate::namespace::UserNamespace,
        _writable_transition: bool,
    ) -> Result<(), FileSystemError> {
        if mount_options::is_empty(options) {
            Ok(())
        } else {
            Err(FileSystemError::InvalidOperation)
        }
    }
    fn shutdown(&self) -> Result<(), FileSystemError> {
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SuperblockFacts {
    pub(crate) type_name: &'static str,
    pub(crate) read_only: bool,
}

pub(crate) struct FileSystemStatistics {
    pub(crate) magic: u64,
    pub(crate) block_size: u64,
    pub(crate) blocks: u64,
    pub(crate) blocks_free: u64,
    pub(crate) blocks_available: u64,
    pub(crate) files: u64,
    pub(crate) files_free: u64,
    pub(crate) fsid: [u32; 2],
    pub(crate) name_length: u64,
    pub(crate) fragment_size: u64,
    pub(crate) flags: u64,
}

#[path = "../../../kernel/src/fs/permission.rs"]
pub(crate) mod permission;
pub(crate) use permission::{AccessIdentity, OwnerModeChange};
#[path = "../../../kernel/src/fs/xattr.rs"]
mod xattr;
pub(crate) use xattr::XattrChange;
// Host ext4 never consumes a memory-backed write token; volatile data owner is tested separately.
pub(crate) mod memory_file {
    pub(crate) struct MemoryWrite<'a>(core::marker::PhantomData<&'a ()>);
}

#[path = "../../../kernel/src/fs/inotify/queue.rs"]
pub(crate) mod inotify_queue;

#[path = "../../../kernel/src/fs/ext4_identity.rs"]
pub(crate) mod ext4_identity;

#[path = "../../../kernel/src/fs/mount_options.rs"]
pub(crate) mod mount_options;

// FUSE 协议与挂载选项的纯编解码；连接与 inode 由 guest 探针在真实 `/dev/fuse` 上覆盖。
#[path = "../../../kernel/src/fs/fuse/options.rs"]
#[allow(
    dead_code,
    reason = "host 只运行选项解析用例，生产调用者在 fuse 文件系统类型中"
)]
mod fuse_options;
#[path = "../../../kernel/src/fs/fuse/protocol.rs"]
#[allow(
    dead_code,
    reason = "host 只运行编解码用例，生产调用者在连接与 inode 中"
)]
mod protocol;

#[path = "../../../kernel/src/fs/tmpfs/options.rs"]
pub(crate) mod tmpfs_options;

#[path = "../../../kernel/src/fs/tmpfs/directory.rs"]
pub(crate) mod tmpfs_directory;

#[path = "../../../tools/kernel-view/fs/ext4/ext4/ext4.rs"]
pub(crate) mod ext4;

/// 与 kernel `fs::SpawnKernelThread` 相同的注入签名；host 测试不创建内核线程。
#[allow(dead_code)]
pub(crate) type SpawnKernelThread =
    fn(&'static str, alloc::boxed::Box<dyn FnOnce() + Send>) -> Result<(), ()>;

/// 与 kernel `fs::KernelThreadSupport` 相同的 task 能力形状。
#[allow(dead_code)]
#[derive(Clone, Copy)]
pub(crate) struct KernelThreadSupport {
    pub(crate) spawn: SpawnKernelThread,
    pub(crate) sleep_until: fn(u64),
}

#[path = "../../../kernel/src/fs/swap/file_lease.rs"]
pub(crate) mod swap;

#[path = "tests/ext4_swap.rs"]
mod swap_tests;

#[allow(
    dead_code,
    reason = "host storage loads actual atime policy; volatile tuple owners have separate integration fixtures"
)]
#[path = "../../../kernel/src/fs/inode_times.rs"]
pub(crate) mod inode_times;

#[path = "../../../kernel/src/fs/write_access.rs"]
mod write_access;
pub(crate) use write_access::{InodeAccessLease, InodeWriteAccess};

#[cfg(test)]
#[path = "tests/inotify_queue.rs"]
mod inotify_queue_tests;

#[cfg(test)]
#[path = "tests/ext4_identity.rs"]
mod ext4_identity_tests;

#[cfg(test)]
#[path = "tests/tmpfs.rs"]
mod tmpfs_tests;
