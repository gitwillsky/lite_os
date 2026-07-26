//! @description Linux memfd anonymous-file storage 与 seal owner。

use alloc::{boxed::Box, sync::Arc, vec::Vec};
use spin::Mutex;

use super::{
    CreateMetadata, DirectoryRead, DirectoryVisitor, FileSystemError, Inode, InodeMetadata,
    InodeType, OpenedFile, OwnerModeChange,
};

#[path = "memfd/state.rs"]
mod state;
use state::{MemFileState, MemFileStateError};
const MEMFD_FILESYSTEM_ID: usize = 6;

/// `memfd_create` 产生的 anonymous regular inode。
pub(crate) struct MemFile {
    inode: u64,
    name: Box<[u8]>,
    state: Mutex<MemFileState>,
}

impl MemFile {
    /// @description 创建空 anonymous file。
    /// @param name `/proc/<pid>/fd` 使用的 Linux memfd diagnostic name。
    /// @param allow_sealing 未设置 `MFD_ALLOW_SEALING` 时初始带 `F_SEAL_SEAL`。
    /// @return 新 memfd inode owner。
    pub(crate) fn new(name: Vec<u8>, allow_sealing: bool) -> Result<Arc<Self>, FileSystemError> {
        Arc::try_new(Self {
            inode: crate::id::next_runtime_object_id(),
            name: name.into_boxed_slice(),
            state: Mutex::new(MemFileState::new(allow_sealing)),
        })
        .map_err(|_| FileSystemError::OutOfMemory)
    }

    /// @description 原子追加受支持 seal。
    /// @param seals `F_SEAL_SEAL|SHRINK|GROW` 子集。
    /// @return 新 seal mask。
    /// @errors 已 sealed 或要求 WRITE/FUTURE_WRITE 等未实现语义时返回明确错误。
    pub(crate) fn add_seals(&self, seals: u32) -> Result<u32, FileSystemError> {
        self.state.lock().add_seals(seals).map_err(state_error)
    }

    pub(crate) fn seals(&self) -> u32 {
        self.state.lock().seals()
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
            blocks: size.div_ceil(512),
            block_size: 4096,
            atime: 0,
            mtime: 0,
            ctime: 0,
            device: None,
        })
    }

    fn inode_type(&self) -> InodeType {
        InodeType::File
    }

    fn size(&self) -> u64 {
        self.state.lock().len() as u64
    }

    fn is_executable(&self) -> bool {
        false
    }

    fn read_storage(&self, offset: u64, output: &mut [u8]) -> Result<usize, FileSystemError> {
        let offset = usize::try_from(offset).map_err(|_| FileSystemError::InvalidOperation)?;
        Ok(self.state.lock().read(offset, output))
    }

    fn write_storage(&self, offset: u64, input: &[u8]) -> Result<usize, FileSystemError> {
        let offset = usize::try_from(offset).map_err(|_| FileSystemError::InvalidOperation)?;
        self.state.lock().write(offset, input).map_err(state_error)
    }

    fn append_storage(&self, input: &[u8]) -> Result<(u64, usize), FileSystemError> {
        let offset = self.size();
        let written = self.write_storage(offset, input)?;
        Ok((offset, written))
    }

    fn truncate_storage(&self, size: u64) -> Result<(), FileSystemError> {
        let size = usize::try_from(size).map_err(|_| FileSystemError::InvalidOperation)?;
        self.state.lock().truncate(size).map_err(state_error)
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

fn state_error(error: MemFileStateError) -> FileSystemError {
    match error {
        MemFileStateError::InvalidOperation => FileSystemError::InvalidOperation,
        MemFileStateError::OutOfMemory => FileSystemError::OutOfMemory,
        MemFileStateError::PermissionDenied => FileSystemError::PermissionDenied,
    }
}
