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
    pub(crate) device: Option<()>,
}

pub(crate) trait StorageWriter {
    fn write(&mut self, offset: u64, bytes: &[u8]) -> Result<usize, FileSystemError>;
}

pub(crate) trait Inode: Send + Sync {
    fn filesystem_id(&self) -> usize;
    fn metadata(&self) -> Result<InodeMetadata, FileSystemError>;
    fn inode_type(&self) -> InodeType;
    fn size(&self) -> u64;
    fn is_executable(&self) -> bool;
    fn read_storage(&self, offset: u64, bytes: &mut [u8]) -> Result<usize, FileSystemError>;
    fn read_link(&self) -> Result<Vec<u8>, FileSystemError>;
    fn write_storage(&self, offset: u64, bytes: &[u8]) -> Result<usize, FileSystemError>;
    fn write_storage_batch(
        &self,
        batch: &mut dyn FnMut(&mut dyn StorageWriter) -> Result<(), FileSystemError>,
    ) -> Result<(), FileSystemError>;
    fn try_write_storage_batch(
        &self,
        batch: &mut dyn FnMut(&mut dyn StorageWriter) -> Result<(), FileSystemError>,
    ) -> Result<(), FileSystemError>;
    fn append_storage(&self, bytes: &[u8]) -> Result<(u64, usize), FileSystemError>;
    fn truncate_storage(&self, size: u64) -> Result<(), FileSystemError>;
    fn allocate_storage(&self, offset: u64, length: u64) -> Result<(), FileSystemError>;
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
    fn root_inode(&self) -> Result<Arc<dyn Inode>, FileSystemError>;
    fn statistics(&self) -> Result<FileSystemStatistics, FileSystemError>;
    fn shutdown(&self) -> Result<(), FileSystemError> {
        Ok(())
    }
}

pub(crate) struct FileSystemStatistics {
    pub(crate) type_name: &'static str,
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

#[derive(Clone, Copy)]
pub(crate) struct OwnerModeChange;

impl OwnerModeChange {
    pub(crate) fn authorize(
        self,
        state: permission::OwnerModeState,
    ) -> Result<permission::OwnerModeState, FileSystemError> {
        Ok(state)
    }
}

pub(crate) mod permission {
    use super::InodeType;

    #[derive(Clone, Copy)]
    pub(crate) struct OwnerModeState {
        mode: u16,
        uid: u32,
        gid: u32,
    }

    impl OwnerModeState {
        pub(crate) const fn new(_: InodeType, mode: u16, uid: u32, gid: u32) -> Self {
            Self { mode, uid, gid }
        }

        pub(crate) const fn mode(self) -> u16 {
            self.mode
        }

        pub(crate) const fn uid(self) -> u32 {
            self.uid
        }

        pub(crate) const fn gid(self) -> u32 {
            self.gid
        }
    }
}

#[path = "../../../kernel/src/fs/block_range.rs"]
pub(crate) mod block_range;

#[cfg(test)]
#[path = "tests/block_range.rs"]
mod block_range_tests;

#[path = "../../../kernel/src/fs/mount_options.rs"]
pub(crate) mod mount_options;

#[path = "../../../kernel/src/fs/tmpfs/options.rs"]
pub(crate) mod tmpfs_options;

#[path = "../../../kernel/src/fs/tmpfs/directory.rs"]
pub(crate) mod tmpfs_directory;

#[cfg(test)]
#[path = "tests/tmpfs.rs"]
mod tmpfs_tests;

#[path = "../../../kernel/src/fs/ext4.rs"]
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
