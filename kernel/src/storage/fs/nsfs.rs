//! nsfs inode与唯一pseudo-superblock；文件只持具体namespace owner，不从task/fd/string重建身份。
use super::{
    CreateMetadata, DataBacking, DirectoryRead, DirectoryVisitor, FileSystemError,
    FileSystemStatistics, Inode, InodeMetadata, InodeType, OpenedFile, OwnerModeChange,
    anonymous::PseudoFileSystem,
    device::{DeviceError, DeviceFile, IoctlCall},
    inode_identity::LiveInodes,
    inode_times::TimeOwner,
    permission::OwnerModeState,
};
use crate::namespace::{NamespaceReference, UserNamespace};
use alloc::sync::{Arc, Weak};
use syscall_abi::errno;

const FILESYSTEM_ID: usize = PseudoFileSystem::Namespace.id();
// OWNER: Weak identity registry 不保活 namespace；实际 inode 最后 Drop 精确摘除 entry。
static INODES: LiveInodes<NamespaceInode, u64> = LiveInodes::new();
struct NamespaceInode {
    namespace: NamespaceReference,
    attributes: spin::Mutex<Attributes>,
    // OWNER: 同一live inode的实际magic dentry弱cache；bind root仍存活时关闭所有FD不丢失身份。
    // 弱边不形成inode→dentry→inode强环，不另建path身份表。
    entry: spin::Mutex<Weak<super::vfs::Dentry>>,
}
struct Attributes {
    owner: OwnerModeState,
    size: u64,
    times: TimeOwner,
}
struct NamespaceFile(NamespaceReference);
impl Drop for NamespaceInode {
    fn drop(&mut self) {
        INODES.retire(self.namespace.inode(), self);
    }
}

/// proc magic link 与 ioctl 共用同一真实 nsfs inode/dentry producer。
pub(crate) fn namespace_opened(
    namespace: NamespaceReference,
) -> Result<Arc<OpenedFile>, FileSystemError> {
    let number = namespace.inode();
    let inode = match INODES.get(number) {
        Some(inode) => inode,
        None => {
            let candidate = Arc::try_new(NamespaceInode {
                namespace,
                entry: spin::Mutex::new(Weak::new()),
                attributes: spin::Mutex::new(Attributes {
                    owner: OwnerModeState::new(InodeType::File, 0o100444, 0, 0),
                    size: 0,
                    times: TimeOwner::new(),
                }),
            })
            .map_err(|_| FileSystemError::OutOfMemory)?;
            INODES.admit(number, &candidate)?
        }
    };
    let existing = inode.entry.lock().upgrade();
    if let Some(existing) = existing {
        return PseudoFileSystem::Namespace.open(existing);
    }
    let name = core::str::from_utf8(inode.namespace.kind().name()).expect("static namespace name");
    let label = super::try_format_bytes(format_args!("{}:[{}]", name, number))?;
    let candidate = super::vfs::Dentry::anonymous(inode.clone(), label)?;
    let (existing, retired) = {
        let mut slot = inode.entry.lock();
        match slot.upgrade() {
            Some(existing) => (Some(existing), None),
            None => (
                None,
                Some(core::mem::replace(&mut *slot, Arc::downgrade(&candidate))),
            ),
        }
    };
    drop(retired); // 最后weak allocation与未采用candidate的Drop均在短锁外。
    PseudoFileSystem::Namespace.open(existing.unwrap_or(candidate))
}

fn visible_owner(owner: &Arc<UserNamespace>, caller: &Arc<UserNamespace>) -> bool {
    let mut current = Some(owner.clone());
    while let Some(namespace) = current {
        if Arc::ptr_eq(&namespace, caller) {
            return true;
        }
        current = namespace.parent();
    }
    false
}
impl Inode for NamespaceInode {
    fn require_access(
        &self,
        identity: &super::AccessIdentity,
        requested: u8,
    ) -> Result<(), FileSystemError> {
        // nsfs不可执行是immutable SB_I_NOEXEC，不随chmod或bind/remount改变。
        if requested & 1 != 0 {
            return Err(FileSystemError::AccessDenied);
        }
        identity.require(self.metadata()?, requested)
    }
    fn filesystem_id(&self) -> usize {
        FILESYSTEM_ID
    }
    fn metadata(&self) -> Result<InodeMetadata, FileSystemError> {
        let attributes = self.attributes.lock();
        let times = attributes.times.snapshot();
        Ok(InodeMetadata {
            filesystem: FILESYSTEM_ID as u64,
            inode: self.namespace.inode(),
            kind: InodeType::File,
            mode: u32::from(attributes.owner.mode()),
            links: 1,
            uid: attributes.owner.uid(),
            gid: attributes.owner.gid(),
            size: attributes.size,
            blocks: 0,
            block_size: 4096,
            atime: times.atime,
            mtime: times.mtime,
            ctime: times.ctime,
            fine_times: Default::default(),
            device: None,
        })
    }
    fn inode_type(&self) -> InodeType {
        InodeType::File
    }
    fn open_backend(
        &self,
        _: u32,
        _: &super::AccessIdentity,
        _: &crate::namespace::NamespaceAuthority,
    ) -> Result<super::InodeOpen, super::InodeOpenError> {
        let backend = Arc::try_new(NamespaceFile(self.namespace.clone()))
            .map_err(|_| FileSystemError::OutOfMemory)?;
        Ok(super::InodeOpen::Device(backend))
    }
    fn namespace_reference(&self) -> Option<NamespaceReference> {
        Some(self.namespace.clone())
    }
    fn size(&self) -> u64 {
        self.attributes.lock().size
    }
    fn is_executable(&self) -> bool {
        false
    }
    fn data_backing(&self) -> DataBacking {
        DataBacking::Snapshot
    }
    fn pseudo_statistics(&self) -> Option<FileSystemStatistics> {
        Some(PseudoFileSystem::Namespace.statistics())
    }
    fn ioctl(&self, call: &IoctlCall<'_>) -> Result<isize, DeviceError> {
        NamespaceFile(self.namespace.clone()).ioctl(call)
    }
    fn read_storage(&self, _: u64, _: &mut [u8]) -> Result<usize, FileSystemError> {
        Err(FileSystemError::InvalidOperation)
    }
    fn write_storage(
        &self,
        _: u64,
        _: &[u8],
        _: Option<&super::AccessIdentity>,
    ) -> Result<usize, FileSystemError> {
        Err(FileSystemError::InvalidOperation)
    }
    fn append_storage(
        &self,
        _: &[u8],
        _: Option<&super::AccessIdentity>,
    ) -> Result<(u64, usize), FileSystemError> {
        Err(FileSystemError::InvalidOperation)
    }
    fn truncate_storage(
        &self,
        size: u64,
        identity: Option<&super::AccessIdentity>,
    ) -> Result<(), FileSystemError> {
        let now = crate::timer::get_realtime_ns() / 1_000_000_000;
        let mut attributes = self.attributes.lock();
        if let Some(identity) = identity {
            let kill = identity.write_kill_mask(
                InodeType::File,
                attributes.owner.mode(),
                attributes.owner.gid(),
            );
            attributes.owner = OwnerModeState::new(
                InodeType::File,
                attributes.owner.mode() & !kill,
                attributes.owner.uid(),
                attributes.owner.gid(),
            );
        }
        attributes.size = size;
        attributes.times.touch_at(now);
        Ok(())
    }
    fn touch_access(&self, now: u64, relatime: bool) -> Result<(), FileSystemError> {
        self.attributes.lock().times.access_at(now, relatime);
        Ok(())
    }
    fn set_times(&self, atime: Option<u64>, mtime: Option<u64>) -> Result<(), FileSystemError> {
        let now = crate::timer::get_realtime_ns() / 1_000_000_000;
        self.attributes.lock().times.set_at(now, atime, mtime);
        Ok(())
    }
    fn change_owner_mode(&self, change: OwnerModeChange) -> Result<(), FileSystemError> {
        let now = crate::timer::get_realtime_ns() / 1_000_000_000;
        let mut attributes = self.attributes.lock();
        attributes.owner = change.authorize(OwnerModeState::new(
            InodeType::File,
            attributes.owner.mode(),
            attributes.owner.uid(),
            attributes.owner.gid(),
        ))?;
        attributes.times.change_at(now);
        Ok(())
    }
    fn sync_storage(&self) -> Result<(), FileSystemError> {
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

// nsfs 只提供 ioctl；没有 llseek/read/mmap callbacks，不能按 inode 的 REG 类型授予 storage 操作。
impl DeviceFile for NamespaceFile {
    fn supports_read(&self) -> bool {
        false
    }
    fn supports_write(&self) -> bool {
        false
    }
    fn poll(&self, events: i16) -> i16 {
        events & 5
    }
    fn ioctl(&self, call: &IoctlCall<'_>) -> Result<isize, DeviceError> {
        let error = |code| DeviceError::Errno(code);
        match call.request {
            0xb701 | 0xb702 => {
                let related = if call.request == 0xb702 {
                    self.0
                        .parent_reference()
                        .map_err(|_| error(errno::EINVAL))?
                } else {
                    self.0.owning_user().map(NamespaceReference::new)
                }
                .ok_or_else(|| error(errno::EPERM))?;
                if related.kind() == crate::namespace::NamespaceType::Pid {
                    let parent = related
                        .downcast::<crate::namespace::PidNamespace>()
                        .map_err(|_| error(errno::EINVAL))?;
                    if !parent.is_descendant_of(&call.user.pid_namespace()) {
                        return Err(error(errno::EPERM));
                    }
                } else {
                    let visibility_owner = related
                        .downcast::<UserNamespace>()
                        .map_err(|_| error(errno::EINVAL))?;
                    if !visible_owner(&visibility_owner, &call.authority.namespace) {
                        return Err(error(errno::EPERM));
                    }
                }
                call.user.install_namespace(related)
            }
            0xb703 => Ok(self.0.kind().flag() as isize),
            0xb704 => {
                let namespace = self
                    .0
                    .downcast::<UserNamespace>()
                    .map_err(|_| error(errno::EINVAL))?;
                let uid = call.authority.namespace.display_id(true, namespace.owner());
                call.user
                    .write(call.argument, &uid.to_ne_bytes())
                    .map_err(|_| error(errno::EFAULT))?;
                Ok(0)
            }
            0x8008_b70d => {
                call.user
                    .write(call.argument, &self.0.id().to_ne_bytes())
                    .map_err(|_| error(errno::EFAULT))?;
                Ok(0)
            }
            _ => Err(error(errno::ENOTTY)),
        }
    }
}
