//! OFD allocation 与成功打开的生命周期分离；失败不能析构尚未成立的 OpenFileDescription。
use alloc::sync::Arc;
use core::{mem::MaybeUninit, sync::atomic::AtomicUsize};
use spin::Mutex;

use super::{
    EpollMemberships, FilePosition, OpenFileDescription, OpenFileKind, WritableInodeLease,
};
use crate::fs::{
    FileSystemError, InodeOpen, InodeType, O_ACCMODE, O_RDONLY, OpenedFile, StorageOpen,
};

/// 具体 anonymous device 建立资源前准备 OFD 与真实 opened identity；finish 不分配。
pub(crate) struct PreparedDeviceDescription {
    description: PreparedFileDescription,
    opened: Arc<OpenedFile>,
    flags: u32,
}
impl PreparedDeviceDescription {
    /// # Errors
    /// OFD/inode/dentry storage OOM；尚未调用设备，也没有实际 close consequence。
    pub(crate) fn try_new(label: &[u8], flags: u32) -> Result<Self, FileSystemError> {
        Ok(Self {
            description: PreparedFileDescription::try_new()?,
            opened: crate::fs::anonymous_opened(label)?,
            flags,
        })
    }
    /// 消费成功创建的唯一 per-open backend，无分配建立标准 OFD。
    pub(crate) fn finish(
        self,
        device: Arc<dyn crate::fs::chardev::DeviceFile>,
    ) -> Arc<OpenFileDescription> {
        self.description.initialize(
            OpenFileKind::Device(device),
            self.opened,
            self.flags,
            None,
            None,
        )
    }
}

/// 文件打开前唯一拥有的 OFD allocation；尚无 backend/close lifecycle，也不可由 fd lookup 取得。
///
/// Token 不 clone、不创建 Weak；失败只释放 MaybeUninit allocation。只有消费式 initialize
/// 完整写入全部 fields 后，才转换为可共享的标准 Arc<OFD>，避免 Arc::try_new(Self) OOM 的伪 close。
pub(crate) struct PreparedFileDescription {
    allocation: Arc<MaybeUninit<OpenFileDescription>>,
}
impl PreparedFileDescription {
    /// 标准 O_PATH 只消费 path/OFD allocation；不取得 writer、打开设备或建立设备 fops。
    pub(crate) fn finish_path(
        self,
        opened: Arc<OpenedFile>,
        flags: u32,
    ) -> Arc<OpenFileDescription> {
        assert!(flags & crate::fs::O_PATH != 0);
        self.initialize(OpenFileKind::Inode, opened, flags, None, None)
    }
    /// 由实际 pseudo filesystem 准备好的 opened/backend 建立 OFD；不另建匿名 inode。
    pub(crate) fn finish_backend(
        self,
        opened: Arc<OpenedFile>,
        backend: Arc<dyn crate::fs::chardev::DeviceFile>,
        flags: u32,
    ) -> Arc<OpenFileDescription> {
        self.initialize(OpenFileKind::Device(backend), opened, flags, None, None)
    }
    /// 在 pathname/create/backend 副作用前 fallible 分配 OFD。
    /// # Errors
    /// Heap OOM 返回 OutOfMemory；不构造或析构任何 OFD。
    pub(crate) fn try_new() -> Result<Self, FileSystemError> {
        Arc::try_new_uninit()
            .map(|allocation| Self { allocation })
            .map_err(|_| FileSystemError::OutOfMemory)
    }

    /// 无分配消费唯一 token 并完整初始化 OFD；caller 已完成所有可能失败的 backend/lease 准备。
    pub(super) fn initialize(
        mut self,
        kind: OpenFileKind,
        opened: Arc<OpenedFile>,
        flags: u32,
        write_open: Option<WritableInodeLease>,
        storage_open: Option<Arc<dyn StorageOpen>>,
    ) -> Arc<OpenFileDescription> {
        Arc::get_mut(&mut self.allocation)
            .expect("unpublished OFD allocation is unique")
            .write(OpenFileDescription {
                kind,
                position: FilePosition::new(),
                flags: Mutex::new(flags),
                opened,
                epoll_memberships: EpollMemberships::new(),
                descriptor_refs: AtomicUsize::new(0),
                _storage_open: storage_open,
                _write_open: write_open,
            });
        // SAFETY: allocation token 从未 clone/Weak/publication；唯一 write 完整初始化 Self，
        // write 后没有可失败步骤，消费 Arc 才建立实际 OFD Drop/close lifecycle。
        unsafe { self.allocation.assume_init() }
    }

    /// 在 backend 打开前登记 named regular/bdev 写者；设备操作与存储操作使用同一 admission。
    /// # Errors
    /// Read-only mount、bdev 冲突、lease OOM 返回原 filesystem error。
    pub(crate) fn prepare_named(
        self,
        opened: Arc<OpenedFile>,
        flags: u32,
    ) -> Result<PreparedNamedFile, FileSystemError> {
        let inode = opened.inode();
        let write_open = if flags & O_ACCMODE != O_RDONLY
            && matches!(inode.inode_type(), InodeType::File | InodeType::BlockDevice)
        {
            let content = opened.begin_content_write()?;
            // bdev admission 失败由同一 RAII lease 撤销已准备的 mount writer。
            let mut guard = WritableInodeLease {
                _content: content,
                block: None,
            };
            if inode.inode_type() == InodeType::BlockDevice {
                let block = inode
                    .device_number()
                    .and_then(crate::fs::devnode::block_node)
                    .ok_or(FileSystemError::NoDevice)?;
                block.begin_writer()?;
                guard.block = Some(block);
            }
            Some(guard)
        } else {
            None
        };
        Ok(PreparedNamedFile {
            allocation: self,
            opened,
            flags,
            write_open,
        })
    }
}

/// 尚未建立 OPEN/CLOSE lifecycle 的 named-file transaction；失败只归还 admission 与 allocation。
/// 写者 lease 在 backend 的任何副作用前取得，regular proc operations 不再绕过 mount owner。
pub(crate) struct PreparedNamedFile {
    allocation: PreparedFileDescription,
    opened: Arc<OpenedFile>,
    flags: u32,
    write_open: Option<WritableInodeLease>,
}
impl PreparedNamedFile {
    /// 消费已成功打开的实际 backend，无分配建立唯一 OFD lifecycle。
    pub(crate) fn finish(self, backend: InodeOpen) -> Arc<OpenFileDescription> {
        let (kind, storage_open) = match backend {
            InodeOpen::Storage(open) => (OpenFileKind::Inode, open),
            InodeOpen::Device(file) => (OpenFileKind::Device(file), None),
        };
        self.allocation
            .initialize(kind, self.opened, self.flags, self.write_open, storage_open)
    }
}
