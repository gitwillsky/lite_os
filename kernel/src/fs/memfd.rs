//! Linux memfd anonymous file：一个只有名字和 seal 语义的 [`MemoryFile`] inode。

use alloc::{boxed::Box, sync::Arc, vec::Vec};

use super::{
    CreateMetadata, DataBacking, DirectoryRead, DirectoryVisitor, FileSystemError, Inode,
    InodeMetadata, InodeType, MemoryFile, OpenedFile, OwnerModeChange, PageBudget,
};
use crate::memory::SharedFileId;

const MEMFD_FILESYSTEM_ID: usize = 6;

/// `memfd_create` 产生的 anonymous regular inode。
pub(crate) struct MemFile {
    inode: u64,
    name: Box<[u8]>,
    file: Arc<MemoryFile>,
}

impl MemFile {
    /// 创建空 anonymous file。
    ///
    /// # Parameters
    ///
    /// - `name`: `/proc/<pid>/fd` 使用的 Linux memfd diagnostic name。
    /// - `allow_sealing`: 未设置 `MFD_ALLOW_SEALING` 时初始带 `F_SEAL_SEAL`。
    ///
    /// # Returns
    ///
    /// 新 memfd inode owner；内容只受物理内存限制。
    pub(crate) fn new(name: Vec<u8>, allow_sealing: bool) -> Result<Arc<Self>, FileSystemError> {
        let inode = crate::id::next_runtime_object_id();
        let file = MemoryFile::new(
            SharedFileId {
                filesystem: MEMFD_FILESYSTEM_ID,
                inode,
            },
            PageBudget::new(None)?,
            allow_sealing,
        )?;
        Arc::try_new(Self {
            inode,
            name: name.into_boxed_slice(),
            file,
        })
        .map_err(|_| FileSystemError::OutOfMemory)
    }

    /// 原子追加受支持 seal。
    ///
    /// # Parameters
    ///
    /// - `seals`: `F_SEAL_SEAL|SHRINK|GROW` 子集。
    ///
    /// # Returns
    ///
    /// 新 seal mask。
    ///
    /// # Errors
    ///
    /// 已 sealed 或要求 WRITE/FUTURE_WRITE 等未实现语义时返回明确错误。
    pub(crate) fn add_seals(&self, seals: u32) -> Result<u32, FileSystemError> {
        self.file.add_seals(seals)
    }

    pub(crate) fn seals(&self) -> Result<u32, FileSystemError> {
        self.file.seals()
    }

    pub(crate) fn name(&self) -> &[u8] {
        &self.name
    }
}

impl Inode for MemFile {
    fn filesystem_id(&self) -> usize {
        MEMFD_FILESYSTEM_ID
    }

    fn metadata(&self) -> Result<InodeMetadata, FileSystemError> {
        let size = self.size();
        Ok(InodeMetadata {
            filesystem: MEMFD_FILESYSTEM_ID as u64,
            inode: self.inode,
            kind: InodeType::File,
            mode: 0o100777,
            links: 0,
            uid: 0,
            gid: 0,
            size,
            // 512-byte 单位的已分配块：稀疏文件的洞不占用。
            blocks: self.file.resident_pages()? as u64 * 8,
            block_size: 4096,
            atime: 0,
            mtime: self.file.modified_seconds(),
            ctime: self.file.modified_seconds(),
            device: None,
        })
    }

    fn inode_type(&self) -> InodeType {
        InodeType::File
    }

    fn size(&self) -> u64 {
        self.file.size()
    }

    fn is_executable(&self) -> bool {
        false
    }

    fn data_backing(&self) -> DataBacking {
        DataBacking::Memory(self.file.clone())
    }

    fn read_storage(&self, offset: u64, output: &mut [u8]) -> Result<usize, FileSystemError> {
        self.file.read(offset, output)
    }

    fn write_storage(&self, offset: u64, input: &[u8]) -> Result<usize, FileSystemError> {
        self.file.begin_write()?.write(offset, input)
    }

    fn append_storage(&self, input: &[u8]) -> Result<(u64, usize), FileSystemError> {
        self.file.begin_write()?.append(input, u64::MAX)
    }

    fn truncate_storage(&self, size: u64) -> Result<(), FileSystemError> {
        self.file.truncate(size)
    }

    fn allocate_storage(&self, offset: u64, length: u64) -> Result<(), FileSystemError> {
        self.file.allocate(offset, length)
    }

    fn sync_storage(&self) -> Result<(), FileSystemError> {
        Ok(())
    }

    fn read_directory(
        &self,
        _cursor: u64,
        _visitor: &mut dyn DirectoryVisitor,
    ) -> Result<DirectoryRead, FileSystemError> {
        Err(FileSystemError::NotDirectory)
    }

    fn find_child(&self, _name: &[u8]) -> Result<Arc<dyn Inode>, FileSystemError> {
        Err(FileSystemError::NotDirectory)
    }

    fn create(
        &self,
        _name: &[u8],
        _kind: InodeType,
        _metadata: CreateMetadata,
    ) -> Result<Arc<dyn Inode>, FileSystemError> {
        Err(FileSystemError::NotDirectory)
    }

    fn change_owner_mode(&self, _change: OwnerModeChange) -> Result<(), FileSystemError> {
        Err(FileSystemError::PermissionDenied)
    }

    fn unlink(&self, _name: &[u8], _remove_directory: bool) -> Result<(), FileSystemError> {
        Err(FileSystemError::NotDirectory)
    }

    fn rename(
        &self,
        _old_name: &[u8],
        _new_parent_inode: u64,
        _new_name: &[u8],
        _no_replace: bool,
    ) -> Result<(), FileSystemError> {
        Err(FileSystemError::NotDirectory)
    }

    fn follow_link(&self) -> Option<Arc<OpenedFile>> {
        None
    }
}
