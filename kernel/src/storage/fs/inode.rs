use alloc::{sync::Arc, vec::Vec};

use super::{
    CreateMetadata, DirectoryRead, DirectoryVisitor, FileSystemError, OpenedFile, OwnerModeChange,
};

#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InodeType {
    File = 0,
    Directory = 1,
    SymLink = 2,
    CharacterDevice = 3,
    BlockDevice = 6,
    Fifo = 4,
    Socket = 5,
}

/// atime/mtime/ctime 的纳秒部分与创建时间。
///
/// `Default` 表示 filesystem 只有秒精度且不记录创建时间；`statx` 据此不报告 `STATX_BTIME`。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct FineTimes {
    pub(crate) atime_nsec: u32,
    pub(crate) mtime_nsec: u32,
    pub(crate) ctime_nsec: u32,
    /// 创建时间（秒，纳秒）。
    pub(crate) birth: Option<(u64, u32)>,
}

/// VFS 与 Linux stat/getdents 共享的稳定 inode 元数据。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    /// 秒以下精度与创建时间；只有秒精度、不记录创建时间的 filesystem 取 `Default`。
    pub(crate) fine_times: FineTimes,
    /// 字符设备的 `st_rdev`；普通 inode 为 `None`。
    pub(crate) device: Option<crate::fs::device::DeviceNumber>,
}

/// 一次 filesystem-owned storage batch 内的顺序 byte writer。
///
/// caller 只提交 offset/bytes；具体 filesystem 决定这些写入共享一个 journal
/// transaction，还是使用默认逐次 storage mutation。writer 不得逃出 batch callback。
pub(crate) trait StorageWriter {
    fn write(&mut self, offset: u64, bytes: &[u8]) -> Result<usize, FileSystemError>;
}

struct DirectStorageWriter<'inode, T: Inode + ?Sized>(&'inode T);

impl<T: Inode + ?Sized> StorageWriter for DirectStorageWriter<'_, T> {
    fn write(&mut self, offset: u64, bytes: &[u8]) -> Result<usize, FileSystemError> {
        self.0.write_storage(offset, bytes, None)
    }
}

/// regular file 内容的存放方式。
pub(crate) enum DataBacking {
    /// 持久文件：经全局 page cache 缓存 filesystem storage。
    PageCache,
    /// 每次读取即时生成的只读快照（procfs）：不缓存、不可 mmap、不可写。
    Snapshot,
    /// 内存型文件（tmpfs、memfd）：页即内容，由该 inode 独占，没有 backing storage。
    Memory(alloc::sync::Arc<super::memory_file::MemoryFile>),
}

/// 一次 inode-owned symlink lookup 的完整结果；不重试目标类型或身份。
///
/// 文本目标由 VFS 在当前 root 下展开；magic 目标直接保留 opened-entry 身份。
pub(crate) enum LinkTarget {
    Text(Vec<u8>),
    Opened(Arc<OpenedFile>),
}

/// inode owner 决定真实打开操作；storage 和专属 file operations 不按 pathname 分流。
pub(crate) enum InodeOpen {
    Storage,
    Device(Arc<dyn super::device::DeviceFile>),
}

/// 打开 storage 的错误与可重启的 peer rendezvous 中断，保留各自领域语义。
pub(crate) enum InodeOpenError {
    Filesystem(FileSystemError),
    Interrupted,
}
impl From<FileSystemError> for InodeOpenError {
    fn from(error: FileSystemError) -> Self {
        Self::Filesystem(error)
    }
}
impl InodeOpenError {
    pub(super) fn from_fifo(error: super::FifoOpenError) -> Self {
        match error {
            super::FifoOpenError::NoReader => FileSystemError::NoDevice.into(),
            super::FifoOpenError::OutOfMemory => FileSystemError::OutOfMemory.into(),
            super::FifoOpenError::Interrupted => Self::Interrupted,
        }
    }
}

/// 唯一 VFS inode 接口，读写和目录变更不保留只读旁路。
pub(crate) trait Inode: Send + Sync {
    /// 原生 proc TGID directory operations 的实际 PID observer；普通 inode/PIDFS/O_PATH 不借标签识别。
    fn process_directory_identity(
        &self,
    ) -> Option<&alloc::sync::Arc<crate::namespace::PidIdentity>> {
        None
    }
    /// 读取实际filesystem-owned xattr；NotFound在xattr ABI边界对应ENODATA。
    fn read_xattr(&self, _name: &[u8]) -> Result<Vec<u8>, super::FileSystemError> {
        Err(super::FileSystemError::NotSupported)
    }
    /// filesystem immutable/append-only事实；只读mount准入由实际path writer lease负责。
    fn check_xattr_write(&self) -> Result<(), super::FileSystemError> {
        Ok(())
    }
    /// NUL分隔的可见attribute名称；trusted属性按实际caller权限过滤。
    fn list_xattrs(
        &self,
        _identity: &super::AccessIdentity,
    ) -> Result<Vec<u8>, super::FileSystemError> {
        Err(super::FileSystemError::NotSupported)
    }
    /// 修改同一实际inode属性；backend在实际mutation gate内按live tuple授权并转换rootid。
    /// None删除、Some(empty)保存空值；失败不改变旧值，成功后ctime归实际inode owner。
    fn change_xattr(
        &self,
        mut change: super::XattrChange<'_>,
    ) -> Result<(), super::FileSystemError> {
        let metadata = self.metadata()?;
        change.prepare(
            &super::permission::OwnerModeState::new(
                metadata.kind,
                metadata.mode as u16,
                metadata.uid,
                metadata.gid,
            ),
            self.check_xattr_write(),
        )?;
        Err(super::FileSystemError::NotSupported)
    }
    /// 真实可写 regular inode 的 canonical access owner；只读 snapshot/特殊 inode 无写访问状态。
    fn write_access(&self) -> Option<&super::InodeWriteAccess> {
        None
    }

    fn filesystem_id(&self) -> usize;

    fn metadata(&self) -> Result<InodeMetadata, FileSystemError>;

    /// inode 实际 permission operation；默认 DAC，proc sysctl 等 owner 可提供自己的 scoped policy。
    fn require_access(
        &self,
        identity: &super::AccessIdentity,
        requested: u8,
    ) -> Result<(), FileSystemError> {
        identity.require(self.metadata()?, requested)
    }

    fn inode_type(&self) -> InodeType;

    fn size(&self) -> u64;

    /// nsfs inode 的实际 owner；普通 inode 不可用于 setns。
    fn namespace_reference(&self) -> Option<crate::namespace::NamespaceReference> {
        None
    }

    /// Live inode swap lease projection; shared writable admission and normal writes consult it.
    fn is_swap_file(&self) -> bool {
        false
    }

    /// Freeze this regular inode before mapped-writer retirement and cache/journal drain.
    /// Unsupported backing returns InvalidOperation; lease cleanup is infallible and allocation-free.
    fn pin_swap_file(
        self: Arc<Self>,
    ) -> Result<alloc::boxed::Box<dyn super::swap::SwapFileLease>, FileSystemError> {
        Err(FileSystemError::InvalidOperation)
    }

    fn is_executable(&self) -> bool;

    /// 在通用 permission 检查之后打开实际操作后端。
    ///
    /// 普通数据 inode 返回 Storage；FIFO/字符设备返回其领域操作。匿名 inode 可以
    /// 显式拒绝 reopen，magic link 仍保留真实 metadata identity，不降级为 label lookup。
    fn open_backend(
        &self,
        flags: u32,
        identity: &super::AccessIdentity,
        _authority: &crate::namespace::NamespaceAuthority,
    ) -> Result<InodeOpen, InodeOpenError> {
        match self.inode_type() {
            InodeType::Fifo => {
                let access = match flags & super::O_ACCMODE {
                    super::O_RDONLY => super::FifoAccess::Read,
                    super::O_WRONLY => super::FifoAccess::Write,
                    _ => super::FifoAccess::ReadWrite,
                };
                let inode = self.metadata()?.inode;
                super::open_fifo(
                    (self.filesystem_id(), inode),
                    access,
                    flags & super::O_NONBLOCK != 0,
                )
                .map(InodeOpen::Device)
                .map_err(InodeOpenError::from_fifo)
            }
            InodeType::CharacterDevice => super::device::open(&super::device::OpenRequest {
                number: self.device_number().ok_or(FileSystemError::NoDevice)?,
                flags,
                identity,
            })
            .map(InodeOpen::Device)
            .map_err(InodeOpenError::Filesystem),
            InodeType::Socket | InodeType::SymLink => Err(FileSystemError::NoDevice.into()),
            InodeType::File | InodeType::Directory | InodeType::BlockDevice => {
                Ok(InodeOpen::Storage)
            }
        }
    }

    /// 不进入 mount namespace 的 pseudo-filesystem 事实；path/fd statfs 共用此 projection。
    fn pseudo_statistics(&self) -> Option<super::FileSystemStatistics> {
        None
    }

    /// MemoryFile 的唯一 seal owner；其他 storage 不支持 seal 操作。
    fn seals(&self) -> Result<u32, FileSystemError> {
        match self.data_backing() {
            DataBacking::Memory(file) => file.seals(),
            _ => Err(FileSystemError::InvalidOperation),
        }
    }

    /// OFD 写访问已由通用 fcntl 校验，本方法只修改 inode content owner 的 seal state。
    fn add_seals(&self, seals: u32) -> Result<u32, FileSystemError> {
        match self.data_backing() {
            DataBacking::Memory(file) => file.add_seals(seals),
            _ => Err(FileSystemError::InvalidOperation),
        }
    }

    /// 该 inode 的内容在 page cache 中的身份。
    ///
    /// 默认是 `(filesystem_id, inode number)`。块设备节点覆盖它：同一块盘可以出现在多个 devtmpfs
    /// 实例里，必须共享同一份缓存，否则两个节点各自缓冲、互相看不到对方的写入。
    ///
    /// # Errors
    ///
    /// metadata 读取失败时透传。
    fn page_cache_id(&self) -> Result<crate::memory::SharedFileId, FileSystemError> {
        Ok(crate::memory::SharedFileId {
            filesystem: self.filesystem_id(),
            inode: self.metadata()?.inode,
        })
    }

    /// inode 专属 ioctl（块设备节点的 `BLK*`）；UAPI 编解码由 inode 所属子系统拥有。
    fn ioctl(
        &self,
        _call: &super::device::IoctlCall<'_>,
    ) -> Result<isize, super::device::DeviceError> {
        Err(super::device::DeviceError::Errno(
            syscall_abi::errno::ENOTTY,
        ))
    }

    /// regular file 内容的存放方式，决定 read/write 与 mmap 走哪条路径。
    ///
    /// 缺少 `Snapshot` 会把第一次 `/proc/stat` 等快照永久缓存，令监控采样冻结；缺少 `Memory` 会
    /// 让内存型文件被 page cache 再缓存一份，并让已删除文件的内容滞留到下一次 `sync`。
    ///
    /// # Returns
    ///
    /// 持久文件为 [`DataBacking::PageCache`]（默认）。
    fn data_backing(&self) -> DataBacking {
        DataBacking::PageCache
    }

    /// notification/last-dput 的最小 inode/link 事实；可变 storage backend 必须无分配、无失败投影。
    /// 默认只供 immutable pseudo inode，普通 tmpfs/ext4 与 block wrapper 从 actual owner 读取。
    fn notification_link_state(&self) -> Option<(u64, u32)> {
        self.metadata()
            .ok()
            .map(|metadata| (metadata.inode, metadata.links))
    }

    /// Linux S_PRIVATE 的 anonymous singleton 抑制 fsnotify；普通 inode/pipe/socket 默认参与。
    fn allows_notifications(&self) -> bool {
        true
    }

    /// 返回 inode 所属 filesystem adapter 是否拒绝持久 mutation。
    ///
    /// # Returns
    ///
    /// 持久只读 adapter 返回 true；proc controls 由实际 inode permission/backend 分别授权。
    fn is_read_only(&self) -> bool {
        false
    }

    /// 经字符设备注册表打开的设备号；普通 filesystem inode 返回 None。
    fn device_number(&self) -> Option<crate::fs::device::DeviceNumber> {
        None
    }

    fn read_storage(&self, offset: u64, buf: &mut [u8]) -> Result<usize, FileSystemError>;

    /// 读取 symbolic-link 的原始 target bytes，不追加 NUL。
    ///
    /// # Returns
    ///
    /// symbolic-link 返回完整 target；其他 inode 默认返回 InvalidOperation。
    fn read_link(&self) -> Result<Vec<u8>, FileSystemError> {
        Err(FileSystemError::InvalidOperation)
    }

    /// 在一次读取中冻结 symbolic-link target，错误直接传播。
    ///
    /// 普通 symlink 使用 raw bytes；proc magic link 返回 live opened-entry，
    /// 避免 descriptor 重用或 OOM 后重试改变目标类别并递归获取 namespace。
    fn link_target(&self) -> Result<LinkTarget, FileSystemError> {
        self.read_link().map(LinkTarget::Text)
    }

    fn write_storage(
        &self,
        offset: u64,
        buf: &[u8],
        identity: Option<&crate::fs::AccessIdentity>,
    ) -> Result<usize, FileSystemError>;

    /// 让 filesystem adapter 在一次 owner-defined storage batch 中消费写入。
    ///
    /// 默认实现只为既有只读/volatile adapter 保持逐次 write_storage 语义；
    /// mutable cached adapter 必须覆盖为 callback 失败时可整体重放的 transaction。
    ///
    /// # Parameters
    ///
    /// - `batch`: 短生命周期 producer；只能通过 StorageWriter 顺序提交 byte ranges。
    ///
    /// # Returns
    ///
    /// producer 与 adapter 全部成功后返回；失败时 caller 不得把 batch 标 clean。
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
    ) -> Result<(), FileSystemError> {
        let mut writer = DirectStorageWriter(self);
        batch(&mut writer)
    }

    /// 尝试在不等待 filesystem mutation owner 的前提下提交回收写回批次。
    ///
    /// # Parameters
    ///
    /// - `batch`: 短生命周期 producer；仅在 adapter 成功取得 mutation ownership 时消费。
    ///
    /// # Returns
    ///
    /// 批次完整提交后返回。
    ///
    /// # Errors
    ///
    /// owner 正忙时返回 Busy 且 batch 未执行；其他 journal、存储或容量错误原样返回。
    fn try_writeback_storage_batch(
        &self,
        _batch: &mut dyn FnMut(&mut dyn StorageWriter) -> Result<(), FileSystemError>,
    ) -> Result<(), FileSystemError> {
        Err(FileSystemError::Busy)
    }

    /// Memory facade 已持有该 MemoryFile 的 write sequence；这里只更新 canonical inode mode。
    fn prepare_memory_write(
        &self,
        _writer: &super::memory_file::MemoryWrite<'_>,
        _identity: Option<&super::AccessIdentity>,
    ) -> Result<(), FileSystemError> {
        Err(FileSystemError::InvalidOperation)
    }

    fn append_storage(
        &self,
        buf: &[u8],
        identity: Option<&crate::fs::AccessIdentity>,
    ) -> Result<(u64, usize), FileSystemError>;

    fn truncate_storage(
        &self,
        size: u64,
        identity: Option<&crate::fs::AccessIdentity>,
    ) -> Result<(), FileSystemError>;

    /// 为 byte range 预分配 backing blocks，不修改已有文件内容。
    ///
    /// # Parameters
    ///
    /// - `offset`: range 起始 byte offset。
    /// - `length`: 非零 range 长度；调用方保证 offset+length 可表示。
    ///
    /// # Returns
    ///
    /// 成功时 range 内不存在 hole，且 i_size 至少到达 range end。
    ///
    /// # Errors
    ///
    /// 非 regular inode、空间不足、只读或底层 I/O 错误。
    fn allocate_storage(
        &self,
        _offset: u64,
        _length: u64,
        _identity: Option<&crate::fs::AccessIdentity>,
    ) -> Result<(), FileSystemError> {
        Err(FileSystemError::InvalidOperation)
    }

    fn sync_storage(&self) -> Result<(), FileSystemError>;

    /// 普通访问的 best-effort 时间更新；实际路径已完成 mount policy/write admission。
    /// 实现只更新 atime；失败不影响已完成的读取。不可变 inode 缺省无时间更新。
    fn touch_access(&self, _now: u64, _relatime: bool) -> Result<(), FileSystemError> {
        Ok(())
    }

    /// 原子更新 inode 的 atime/mtime，并由 filesystem 更新 ctime。
    ///
    /// # Parameters
    ///
    /// - `atime`: Some 为新的 epoch seconds，None 保留现值。
    /// - `mtime`: Some 为新的 epoch seconds，None 保留现值。
    ///
    /// # Returns
    ///
    /// 成功或底层只读、I/O 错误；不支持 mutation 的 inode 默认返回 ReadOnly。
    fn set_times(&self, atime: Option<u64>, mtime: Option<u64>) -> Result<(), FileSystemError> {
        if atime.is_none() && mtime.is_none() {
            Ok(())
        } else {
            Err(FileSystemError::ReadOnly)
        }
    }

    /// 从 opaque directory cursor 开始向 visitor 投递 live entries。
    ///
    /// # Parameters
    ///
    /// - `cursor`: 上次成功发布的 `d_off`，零表示从头开始。
    /// - `visitor`: 同步消费 borrowed entry；Stop 时当前 entry 不得推进 cursor。
    ///
    /// # Returns
    ///
    /// 下一 cursor 与 EOF；adapter 不得先构造完整目录快照。
    ///
    /// # Errors
    ///
    /// 非目录、底层 I/O、损坏布局或 visitor 编码错误。
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

    /// 在当前目录创建 FIFO 或设备节点（Linux `mknod`）。
    ///
    /// # Parameters
    ///
    /// - `kind`: [`InodeType::Fifo`]、[`InodeType::CharacterDevice`] 或 [`InodeType::BlockDevice`]。
    /// - `device`: 设备节点的设备号；FIFO 为 `None`。
    ///
    /// # Errors
    ///
    /// 默认实现（只读或不支持特殊文件的 filesystem）返回 `ReadOnly`；实现对 `kind` 与 `device` 不匹配
    /// 返回 `InvalidOperation`，名字重复、空间不足等返回对应错误。
    fn mknod(
        &self,
        _name: &[u8],
        _kind: InodeType,
        _metadata: CreateMetadata,
        _device: Option<super::device::DeviceNumber>,
    ) -> Result<Arc<dyn Inode>, FileSystemError> {
        Err(FileSystemError::ReadOnly)
    }

    /// 在 filesystem mutation owner 内按 live state 原子授权并持久化 chmod/chown。
    ///
    /// # Parameters
    ///
    /// - `change`: 调用身份与已解码的 mode/UID/GID 语义请求。
    ///
    /// # Returns
    ///
    /// 成功或权限、只读、范围、I/O 错误。
    fn change_owner_mode(&self, change: OwnerModeChange) -> Result<(), FileSystemError> {
        change.authorize_metadata(self.metadata()?)?;
        Err(FileSystemError::ReadOnly)
    }

    /// 在当前目录创建保存 raw target bytes 的 symbolic link。
    ///
    /// # Parameters
    ///
    /// - `name`: 新目录项名称。
    /// - `target`: 不含结尾 NUL 的 symbolic-link target。
    ///
    /// # Returns
    ///
    /// 新 symbolic-link inode。
    ///
    /// # Errors
    ///
    /// 名称、空间、只读或底层 I/O 错误。
    fn symlink(
        &self,
        _name: &[u8],
        _target: &[u8],
        _metadata: CreateMetadata,
    ) -> Result<Arc<dyn Inode>, FileSystemError> {
        Err(FileSystemError::ReadOnly)
    }

    /// 在当前目录为同一 filesystem 的非目录 inode 创建硬链接。
    ///
    /// # Parameters
    ///
    /// - `name`: 新目录项名称。
    /// - `target`: VFS 已解析且保持存活的目标 inode。
    ///
    /// # Returns
    ///
    /// 成功或明确的目录项/link-count 错误。
    ///
    /// # Errors
    ///
    /// 跨 filesystem、目录目标、link-count 溢出、只读或底层 I/O 错误。
    fn link(&self, _name: &[u8], _target: Arc<dyn Inode>) -> Result<(), FileSystemError> {
        Err(FileSystemError::ReadOnly)
    }

    fn unlink(&self, name: &[u8], remove_directory: bool) -> Result<(), FileSystemError>;

    fn rename(
        &self,
        old_name: &[u8],
        new_parent_inode: u64,
        new_name: &[u8],
        no_replace: bool,
    ) -> Result<(), FileSystemError>;
}
