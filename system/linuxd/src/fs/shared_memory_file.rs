//! 共享匿名内存的真实内部 tmpfs inode/path；内容仍是提供者交出的同一个匿名 VMO。
use super::{
    AccessIdentity, CreateMetadata, DataBacking, DirectoryRead, DirectoryVisitor, FileSystemError,
    FileSystemStatistics, Inode, InodeMetadata, InodeType, OpenedFile, OwnerModeChange,
    anonymous::{Metadata, PseudoFileSystem},
    inode_times::TimeOwner,
    page_cache::status_error,
};
use crate::memory::MemoryObject;
use alloc::{sync::Arc, vec::Vec};

/// SysV 段与共享匿名映射的真实文件身份；文件观察引用不产生附加 membership。
/// 不保活 SharedMemoryBacking/segment，避免 file → backing → file 的环。
pub(crate) struct SharedMemoryFile {
    metadata: Metadata,
    object: Arc<MemoryObject>,
    length: u64,
}

impl SharedMemoryFile {
    /// 在内部 tmpfs 建立段文件；不复制页、不创建第二个 VMO，也不登记 VMA membership。
    ///
    /// # Parameters
    /// - `label`: 实际提供者给出的诊断 dentry（SYSV key 或 /dev/zero），不用于解析对象身份。
    /// - `length`: 段的原始字节长度，object 已覆盖向上取整后的页。
    /// - `object`: 唯一内容对象的共享引用。
    /// - `creator`: 文件 FSCREDS，与 IPC 权限 owner 分别归属各自的契约。
    ///
    /// # Returns
    /// 保活 inode、内部挂载与原内容的实际打开路径。
    ///
    /// # Errors
    /// 身份、标签或路径分配失败返回 OutOfMemory；未发表的引用自动回收。
    pub(crate) fn create_opened(
        label: Vec<u8>,
        length: u64,
        object: Arc<MemoryObject>,
        creator: &AccessIdentity,
    ) -> Result<Arc<OpenedFile>, FileSystemError> {
        let inode = Arc::try_new(Self {
            metadata: Metadata::new(
                PseudoFileSystem::Memory,
                crate::id::next_runtime_object_id(),
                InodeType::File,
                CreateMetadata {
                    mode: 0o100777,
                    uid: creator.uid(),
                    gid: creator.gid(),
                },
                0,
                TimeOwner::new(),
            ),
            object,
            length,
        })
        .map_err(|_| FileSystemError::OutOfMemory)?;
        OpenedFile::anonymous(PseudoFileSystem::Memory, inode, label)
    }
}

impl crate::memory::MappedFileOwner for OpenedFile {}

impl Inode for SharedMemoryFile {
    fn is_executable(&self) -> bool {
        self.metadata.snapshot(self.length, 0).mode & 0o111 != 0
    }
    fn filesystem_id(&self) -> usize {
        PseudoFileSystem::Memory.id()
    }
    fn identity(&self) -> Result<(u64, u64), FileSystemError> {
        Ok(self.metadata.identity())
    }
    fn inode_type(&self) -> InodeType {
        InodeType::File
    }
    fn size(&self) -> u64 {
        self.length
    }
    fn metadata(&self) -> Result<InodeMetadata, FileSystemError> {
        let bytes = self
            .object
            .committed_bytes()
            .map_err(|_| FileSystemError::IoError)?;
        Ok(self.metadata.snapshot(self.length, bytes / 512))
    }
    fn pseudo_statistics(&self) -> Option<FileSystemStatistics> {
        Some(PseudoFileSystem::Memory.statistics())
    }
    fn data_backing(&self) -> DataBacking {
        DataBacking::SharedVmo
    }
    fn backing_memory(&self) -> Result<native_runtime::Vmo, FileSystemError> {
        self.object
            .vmo()
            .duplicate(native_runtime::native_abi::rights::SAME)
            .map_err(status_error)
    }
    fn read_storage(&self, offset: u64, output: &mut [u8]) -> Result<usize, FileSystemError> {
        let count = output
            .len()
            .min(self.length.saturating_sub(offset) as usize);
        if count != 0 {
            self.object
                .vmo()
                .read(&mut output[..count], offset)
                .map_err(status_error)?;
        }
        Ok(count)
    }
    fn write_storage(
        &self,
        offset: u64,
        input: &[u8],
        _: Option<&AccessIdentity>,
    ) -> Result<usize, FileSystemError> {
        let count = input.len().min(self.length.saturating_sub(offset) as usize);
        if count == 0 && !input.is_empty() {
            return Err(FileSystemError::NoSpace);
        }
        if count != 0 {
            self.object
                .vmo()
                .write(&input[..count], offset)
                .map_err(status_error)?;
        }
        Ok(count)
    }
    fn append_storage(
        &self,
        _: &[u8],
        _: Option<&AccessIdentity>,
    ) -> Result<(u64, usize), FileSystemError> {
        Err(FileSystemError::NotSupported)
    }
    fn truncate_storage(
        &self,
        size: u64,
        _: Option<&AccessIdentity>,
    ) -> Result<(), FileSystemError> {
        if size == self.length {
            Ok(())
        } else {
            Err(FileSystemError::NotSupported)
        }
    }
    fn sync_storage(&self) -> Result<(), FileSystemError> {
        Ok(())
    }
    fn change_owner_mode(&self, change: OwnerModeChange) -> Result<(), FileSystemError> {
        self.metadata.change(&change)
    }
    fn set_times(&self, atime: Option<u64>, mtime: Option<u64>) -> Result<(), FileSystemError> {
        self.metadata.set_times(atime, mtime);
        Ok(())
    }
    fn read_directory(
        &self,
        _: u64,
        _: &mut dyn DirectoryVisitor,
    ) -> Result<DirectoryRead, FileSystemError> {
        Err(FileSystemError::NotDirectory)
    }
    fn find_child(&self, _: &[u8]) -> Result<Arc<dyn Inode>, FileSystemError> {
        Err(FileSystemError::NotDirectory)
    }
    fn create(
        &self,
        _: &[u8],
        _: InodeType,
        _: CreateMetadata,
    ) -> Result<Arc<dyn Inode>, FileSystemError> {
        Err(FileSystemError::NotDirectory)
    }
    fn unlink(&self, _: &[u8], _: bool) -> Result<(), FileSystemError> {
        Err(FileSystemError::NotDirectory)
    }
    fn rename(&self, _: &[u8], _: u64, _: &[u8], _: bool) -> Result<(), FileSystemError> {
        Err(FileSystemError::NotDirectory)
    }
}
