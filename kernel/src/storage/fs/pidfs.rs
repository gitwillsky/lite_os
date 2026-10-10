//! 标准 PIDFD 的真实 pidfs inode 与 file operations；PID association 生命周期只查询 task owner。
use super::{
    AccessIdentity, CreateMetadata, DataBacking, DirectoryRead, DirectoryVisitor, FileSystemError,
    FileSystemStatistics, Inode, InodeMetadata, InodeOpen, InodeOpenError, InodeType, OpenedFile,
    OwnerModeChange,
    anonymous::{Metadata, PseudoFileSystem},
    device::{DeviceError, DeviceFile, DeviceWaitSource, DeviceWaitSources, IoctlCall},
    inode_identity::LiveInodes,
};
use crate::{
    ipc::{Pipe, PipeDirection, PipeEnd},
    namespace::PidIdentity,
};
use alloc::sync::{Arc, Weak};

#[path = "pidfs/binding.rs"]
mod binding;
#[path = "pidfs/ioctl.rs"]
mod ioctl;
pub(crate) use binding::{PidFdPoll, PidFdTarget};

/// 一个 actual PID 的 lazy notification source；graph 只持 Weak，inode/OFD pin 实际 endpoints。
pub(crate) struct PidFdSource {
    read: Arc<PipeEnd>,
    write: Arc<PipeEnd>,
    // Linux pidfs_attr 的 immutable exit information；只在实际 PID association 退休时写入。
    // 不镜像 poll/alive phase，不保活 Task/MM；inode 与 PIDFD observer 共享这一份历史记录。
    exit_status: spin::Mutex<Option<i32>>,
}
impl PidFdSource {
    pub(crate) fn try_new() -> Result<Arc<Self>, FileSystemError> {
        let (read, write) = Pipe::notification_pair().map_err(|_| FileSystemError::OutOfMemory)?;
        Arc::try_new(Self {
            read,
            write,
            exit_status: spin::Mutex::new(None),
        })
        .map_err(|_| FileSystemError::OutOfMemory)
    }
    pub(crate) fn notify(&self) {
        self.write.signal_readiness();
    }
    /// 实际 association retirement token 记录历史退出信息，然后通知；不镜像 poll phase。
    pub(crate) fn record_exit(&self, status: i32) {
        assert!(
            self.exit_status.lock().replace(status).is_none(),
            "PID exit information published twice"
        );
    }
    pub(crate) fn exit_status(&self) -> Option<i32> {
        *self.exit_status.lock()
    }
    pub(crate) fn pipe(&self) -> Arc<Pipe> {
        self.read.pipe()
    }
    pub(crate) fn generation(&self) -> u64 {
        self.read.pipe().readiness_generation(PipeDirection::Read)
    }
}

// OWNER: weak identity admission 不延长执行体、namespace PID association 或打开文件；final Drop 精确退休。
static INODES: LiveInodes<PidInode, usize> = LiveInodes::new();
type OpenedPidFile = (Arc<OpenedFile>, Arc<dyn DeviceFile>, Arc<PidFdSource>);

struct PidInode {
    target: Arc<dyn PidFdTarget>,
    metadata: Metadata,
    entry: spin::Mutex<Weak<super::vfs::Dentry>>,
}
impl Drop for PidInode {
    fn drop(&mut self) {
        INODES.retire(self.target.identity().key(), self);
    }
}
struct PidFile(Arc<dyn PidFdTarget>);
impl DeviceFile for PidFile {
    fn pid_reference(&self) -> Option<&Arc<PidIdentity>> {
        Some(self.0.identity())
    }
    fn supports_read(&self) -> bool {
        false
    }
    fn supports_write(&self) -> bool {
        false
    }
    fn write_file_information(
        &self,
        view: &crate::namespace::PidNamespace,
        output: &mut dyn core::fmt::Write,
    ) -> core::fmt::Result {
        let number = if self.0.poll() == PidFdPoll::Reaped {
            -1
        } else {
            i64::from(self.0.identity().number_in(view))
        };
        write!(output, "Pid:\t{number}\nNSpid:\t{number}")?;
        if number > 0 {
            for descendant in self.0.identity().numbers_in(view).skip(1) {
                write!(output, "\t{descendant}")?;
            }
        }
        writeln!(output)
    }
    fn anonymous_name(&self) -> &'static [u8] {
        b"anon_inode:[pidfd]"
    }
    fn poll(&self, events: i16) -> i16 {
        let state = self.0.poll();
        let input = if state != PidFdPoll::Running {
            events & (0x001 | 0x040)
        } else {
            0
        };
        input | if state == PidFdPoll::Reaped { 0x010 } else { 0 }
    }
    fn wait_sources(&self, _events: i16) -> DeviceWaitSources {
        let mut sources = DeviceWaitSources::new();
        sources.push(DeviceWaitSource::Pipe {
            pipe: self.0.source().pipe(),
            direction: PipeDirection::Read,
            events: 0x001 | 0x040 | 0x010,
        });
        sources
    }
    fn readiness_generation(&self) -> u64 {
        self.0.source().generation()
    }
    fn prepare_wait(&self, events: i16) -> Option<Arc<Pipe>> {
        if self.poll(events) != 0 {
            return None;
        }
        self.0.source().read.drain_readiness();
        if self.poll(events) != 0 {
            None
        } else {
            Some(self.0.source().pipe())
        }
    }
    fn ioctl(&self, call: &IoctlCall<'_>) -> Result<isize, DeviceError> {
        ioctl::ioctl(self.0.as_ref(), call)
    }
}

/// 准备标准 inode/dentry/backend；所有失败先于 fd/child graph publication。
/// 同一 actual PID 的所有打开方式共用唯一 live inode 的属性与 source，不按数字 PID 建索引。
pub(crate) fn prepare_open(
    target: Arc<dyn PidFdTarget>,
    creator: &AccessIdentity,
) -> Result<OpenedPidFile, FileSystemError> {
    let key = target.identity().key();
    let inode = match INODES.get(key) {
        Some(inode) => inode,
        None => {
            let candidate = Arc::try_new(PidInode {
                target,
                metadata: Metadata::new(
                    PseudoFileSystem::Pid,
                    key as u64,
                    InodeType::File,
                    CreateMetadata {
                        mode: 0o700,
                        uid: creator.uid(),
                        gid: creator.gid(),
                    },
                    1,
                    super::inode_times::TimeOwner::new(),
                ),
                entry: spin::Mutex::new(Weak::new()),
            })
            .map_err(|_| FileSystemError::OutOfMemory)?;
            INODES.admit(key, &candidate)?
        }
    };
    let existing = inode.entry.lock().upgrade();
    let entry = if let Some(entry) = existing {
        entry
    } else {
        let label = super::try_format_bytes(format_args!("anon_inode:[pidfd]"))?;
        let candidate = super::vfs::Dentry::anonymous(inode.clone(), label)?;
        let (entry, retired) = {
            let mut slot = inode.entry.lock();
            match slot.upgrade() {
                Some(entry) => (entry, None),
                None => (
                    candidate.clone(),
                    Some(core::mem::replace(&mut *slot, Arc::downgrade(&candidate))),
                ),
            }
        };
        drop(retired);
        entry
    };
    let opened = PseudoFileSystem::Pid.open(entry)?;
    let file =
        Arc::try_new(PidFile(inode.target.clone())).map_err(|_| FileSystemError::OutOfMemory)?;
    Ok((opened, file, inode.target.source().clone()))
}

impl Inode for PidInode {
    fn filesystem_id(&self) -> usize {
        PseudoFileSystem::Pid.id()
    }
    fn metadata(&self) -> Result<InodeMetadata, FileSystemError> {
        Ok(self.metadata.snapshot(0, 0))
    }
    fn inode_type(&self) -> InodeType {
        InodeType::File
    }
    fn size(&self) -> u64 {
        0
    }
    fn is_executable(&self) -> bool {
        false
    }
    fn allows_notifications(&self) -> bool {
        false
    }
    fn data_backing(&self) -> DataBacking {
        DataBacking::Snapshot
    }
    fn pseudo_statistics(&self) -> Option<FileSystemStatistics> {
        Some(PseudoFileSystem::Pid.statistics())
    }
    fn open_backend(
        &self,
        _flags: u32,
        _creator: &AccessIdentity,
        _authority: &crate::namespace::NamespaceAuthority,
    ) -> Result<InodeOpen, InodeOpenError> {
        let file =
            Arc::try_new(PidFile(self.target.clone())).map_err(|_| FileSystemError::OutOfMemory)?;
        Ok(InodeOpen::Device(file))
    }
    fn ioctl(&self, call: &IoctlCall<'_>) -> Result<isize, DeviceError> {
        ioctl::ioctl(self.target.as_ref(), call)
    }
    fn read_storage(&self, _: u64, _: &mut [u8]) -> Result<usize, FileSystemError> {
        Err(FileSystemError::NoDevice)
    }
    fn write_storage(
        &self,
        _: u64,
        _: &[u8],
        _: Option<&AccessIdentity>,
    ) -> Result<usize, FileSystemError> {
        Err(FileSystemError::NoDevice)
    }
    fn append_storage(
        &self,
        _: &[u8],
        _: Option<&AccessIdentity>,
    ) -> Result<(u64, usize), FileSystemError> {
        Err(FileSystemError::NoDevice)
    }
    fn truncate_storage(&self, _: u64, _: Option<&AccessIdentity>) -> Result<(), FileSystemError> {
        Err(FileSystemError::InvalidOperation)
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
    fn change_owner_mode(&self, change: OwnerModeChange) -> Result<(), FileSystemError> {
        self.metadata.change(&change)
    }
    fn set_times(&self, atime: Option<u64>, mtime: Option<u64>) -> Result<(), FileSystemError> {
        self.metadata.set_times(atime, mtime);
        Ok(())
    }
}
