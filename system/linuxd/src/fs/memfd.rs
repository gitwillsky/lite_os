//! Linux memfd 的真实 regular inode；诊断名字属于 opened dentry，内容和 seals 属于 MemoryFile。

use alloc::{sync::Arc, vec::Vec};

use super::{
    AccessIdentity, CreateMetadata, DataBacking, DirectoryRead, DirectoryVisitor, FileSystemError,
    FileSystemStatistics, Inode, InodeMetadata, InodeType, MemoryFile, OpenedFile, OwnerModeChange,
    StorageBudget, anonymous::PseudoFileSystem, permission::OwnerModeState, xattr::Xattrs,
};
use crate::execution::sync::{TaskMutex, TaskMutexGuard, TaskMutexWaitPreparation};
use crate::memory::SharedFileId;

/// 实际memfd inode的唯一attribute owner。沿MemoryFile write sequence→state→times锁序；
/// 属性分配/namespace授权只在task mutex内进行，不在旧pseudo metadata spin临界区执行。
struct MemFileState {
    owner: OwnerModeState,
    xattrs: Xattrs,
    times: super::inode_times::TimeOwner,
}

/// `memfd_create` 产生的 anonymous regular inode。
pub(crate) struct MemFile {
    inode: u64,
    state: TaskMutex<MemFileState>,
    file: Arc<MemoryFile>,
    write_access: super::InodeWriteAccess,
}

impl MemFile {
    fn state(&self) -> Result<TaskMutexGuard<'_, MemFileState>, FileSystemError> {
        self.state.lock().map_err(|_| FileSystemError::OutOfMemory)
    }
    fn kill_write_privileges(state: &mut MemFileState, identity: &AccessIdentity) {
        let kill = identity.write_kill_mask(InodeType::File, state.owner.mode(), state.owner.gid());
        let capability_removed = state.xattrs.kill_capability().is_some();
        if kill != 0 || capability_removed {
            state.owner = OwnerModeState::new(
                InodeType::File,
                state.owner.mode() & !kill,
                state.owner.uid(),
                state.owner.gid(),
            );
            state
                .times
                .change_at(crate::execution::timer::get_realtime_ns() / 1_000_000_000);
        }
    }
    /// 用 creator FSCREDS 创建空 regular inode 及唯一 diagnostic dentry。
    ///
    /// `allow_sealing` 决定初始 F_SEAL_SEAL；名字不进入 namespace 索引。
    /// 分配失败返回 OutOfMemory，未发布的 content/inode 由 Arc 回收。
    pub(crate) fn create_opened(
        name: Vec<u8>,
        allow_sealing: bool,
        creator: &AccessIdentity,
    ) -> Result<Arc<OpenedFile>, FileSystemError> {
        let inode = crate::id::next_runtime_object_id();
        let file = MemoryFile::new(
            SharedFileId {
                filesystem: PseudoFileSystem::Memory.id(),
                inode,
            },
            StorageBudget::new(None, None)?,
            allow_sealing,
        )?;
        let mut label = Vec::new();
        label
            .try_reserve_exact(7 + name.len() + 10)
            .map_err(|_| FileSystemError::OutOfMemory)?;
        label.extend_from_slice(b"/memfd:");
        label.extend_from_slice(&name);
        label.extend_from_slice(b" (deleted)");
        let inode = Arc::try_new(Self {
            inode,
            state: TaskMutex::new(MemFileState {
                owner: OwnerModeState::new(InodeType::File, 0o100777, creator.uid(), creator.gid()),
                xattrs: Xattrs::new(),
                times: super::inode_times::TimeOwner::Memory(file.times()),
            }),
            file,
            write_access: super::InodeWriteAccess::new(),
        })
        .map_err(|_| FileSystemError::OutOfMemory)?;
        OpenedFile::anonymous(PseudoFileSystem::Memory, inode, label)
    }
}

impl Inode for MemFile {
    fn read_xattr(&self, name: &[u8]) -> Result<Vec<u8>, FileSystemError> {
        self.state()?.xattrs.get(name)
    }
    fn list_xattrs(&self, identity: &AccessIdentity) -> Result<Vec<u8>, FileSystemError> {
        let trusted = identity.has_capability(syscall_abi::capability::Capability::SYS_ADMIN);
        self.state()?
            .xattrs
            .list(|name| trusted || !name.starts_with(b"trusted."))
    }
    fn change_xattr(&self, mut change: super::XattrChange<'_>) -> Result<(), FileSystemError> {
        let _writer = self.file.begin_write()?;
        let mut state = self.state()?;
        change.prepare(&state.owner, Ok(()))?;
        let retired = state
            .xattrs
            .change(change.name, change.value, change.flags)?;
        state
            .times
            .change_at(crate::execution::timer::get_realtime_ns() / 1_000_000_000);
        drop(state);
        drop(retired);
        Ok(())
    }
    fn write_access(&self) -> Option<&super::InodeWriteAccess> {
        Some(&self.write_access)
    }

    fn filesystem_id(&self) -> usize {
        PseudoFileSystem::Memory.id()
    }

    fn metadata(&self) -> Result<InodeMetadata, FileSystemError> {
        let state = self.state()?;
        let times = state.times.snapshot();
        Ok(InodeMetadata {
            filesystem: PseudoFileSystem::Memory.id() as u64,
            inode: self.inode,
            kind: InodeType::File,
            mode: u32::from(state.owner.mode()),
            links: 0,
            uid: state.owner.uid(),
            gid: state.owner.gid(),
            size: self.size(),
            blocks: self.file.allocated_pages()? as u64 * 8,
            block_size: 4096,
            atime: times.atime,
            mtime: times.mtime,
            ctime: times.ctime,
            fine_times: Default::default(),
            device: None,
        })
    }

    fn pseudo_statistics(&self) -> Option<FileSystemStatistics> {
        Some(PseudoFileSystem::Memory.statistics())
    }

    fn inode_type(&self) -> InodeType {
        InodeType::File
    }

    fn size(&self) -> u64 {
        self.file.size()
    }

    fn is_executable(&self) -> bool {
        self.state()
            .is_ok_and(|state| state.owner.mode() & 0o111 != 0)
    }

    fn data_backing(&self) -> DataBacking {
        DataBacking::Memory(self.file.clone())
    }

    fn read_storage(&self, offset: u64, output: &mut [u8]) -> Result<usize, FileSystemError> {
        self.file.read(offset, output)
    }

    fn write_storage(
        &self,
        offset: u64,
        input: &[u8],
        identity: Option<&crate::fs::AccessIdentity>,
    ) -> Result<usize, FileSystemError> {
        let writer = self.file.begin_write()?;
        if !input.is_empty() {
            self.prepare_memory_write(&writer, identity)?;
        }
        writer.write(offset, input)
    }

    fn prepare_memory_write(
        &self,
        writer: &super::memory_file::MemoryWrite<'_>,
        identity: Option<&super::AccessIdentity>,
    ) -> Result<(), FileSystemError> {
        if !writer.belongs_to(&self.file) {
            return Err(FileSystemError::InvalidOperation);
        }
        if let Some(identity) = identity {
            let mut state = self.state()?;
            Self::kill_write_privileges(&mut state, identity);
        }
        Ok(())
    }

    fn append_storage(
        &self,
        input: &[u8],
        identity: Option<&crate::fs::AccessIdentity>,
    ) -> Result<(u64, usize), FileSystemError> {
        let writer = self.file.begin_write()?;
        if !input.is_empty() {
            self.prepare_memory_write(&writer, identity)?;
        }
        writer.append(input, u64::MAX)
    }

    fn truncate_storage(
        &self,
        size: u64,
        identity: Option<&crate::fs::AccessIdentity>,
    ) -> Result<(), FileSystemError> {
        self.file.truncate_with_metadata(
            size,
            || {
                if identity.is_some() {
                    self.state().map(Some)
                } else {
                    Ok(None)
                }
            },
            |state| {
                if let (Some(state), Some(identity)) = (state.as_mut(), identity) {
                    Self::kill_write_privileges(state, identity);
                }
            },
        )
    }

    fn allocate_storage(
        &self,
        offset: u64,
        length: u64,
        identity: Option<&crate::fs::AccessIdentity>,
    ) -> Result<(), FileSystemError> {
        // 页发布后不能再因metadata waiter分配失败保留capability，预留等待节点只供caller路径。
        let mut wait = identity
            .map(|_| TaskMutexWaitPreparation::prepare())
            .transpose()
            .map_err(|_| FileSystemError::OutOfMemory)?;
        self.file.allocate_with_metadata(offset, length, || {
            if let (Some(identity), Some(wait)) = (identity, wait.as_mut()) {
                Self::kill_write_privileges(&mut self.state.lock_prepared(wait), identity);
            }
        })
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

    fn change_owner_mode(&self, change: OwnerModeChange) -> Result<(), FileSystemError> {
        let _writer = self.file.begin_write()?;
        let mut state = self.state()?;
        let owner = &state.owner;
        let updated = change.authorize(OwnerModeState::new(
            InodeType::File,
            owner.mode(),
            owner.uid(),
            owner.gid(),
        ))?;
        state.owner = updated;
        let retired = if change.kills_privileges(InodeType::File) {
            state.xattrs.kill_capability()
        } else {
            None
        };
        state
            .times
            .change_at(crate::execution::timer::get_realtime_ns() / 1_000_000_000);
        drop(state);
        drop(retired);
        Ok(())
    }

    fn touch_access(&self, now: u64, relatime: bool) -> Result<(), FileSystemError> {
        self.state()?.times.access_at(now, relatime);
        Ok(())
    }
    fn set_times(&self, atime: Option<u64>, mtime: Option<u64>) -> Result<(), FileSystemError> {
        let _writer = self.file.begin_write()?;
        self.state()?.times.set_at(
            crate::execution::timer::get_realtime_ns() / 1_000_000_000,
            atime,
            mtime,
        );
        Ok(())
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
}
