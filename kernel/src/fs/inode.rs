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
        self.0.write_storage(offset, bytes)
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

/// 唯一 VFS inode 接口，读写和目录变更不保留只读旁路。
pub(crate) trait Inode: Send + Sync {
    fn filesystem_id(&self) -> usize;

    fn metadata(&self) -> Result<InodeMetadata, FileSystemError>;

    fn inode_type(&self) -> InodeType;

    fn size(&self) -> u64;

    fn is_executable(&self) -> bool;

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

    /// 返回 inode 所属 filesystem adapter 是否拒绝持久 mutation。
    ///
    /// # Returns
    ///
    /// ext4 root 为 false；只读 devfs/procfs 为 true。
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

    /// 解析 procfs 等 kernel-owned magic link 的 live opened-entry target。
    ///
    /// # Returns
    ///
    /// magic link 返回目标；persistent/devfs 普通 symlink 返回 None 并使用 raw bytes。
    fn follow_link(&self) -> Option<Arc<OpenedFile>> {
        None
    }

    fn write_storage(&self, offset: u64, buf: &[u8]) -> Result<usize, FileSystemError>;

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
    fn write_storage_batch(
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
    fn try_write_storage_batch(
        &self,
        _batch: &mut dyn FnMut(&mut dyn StorageWriter) -> Result<(), FileSystemError>,
    ) -> Result<(), FileSystemError> {
        Err(FileSystemError::Busy)
    }

    fn append_storage(&self, buf: &[u8]) -> Result<(u64, usize), FileSystemError>;

    fn truncate_storage(&self, size: u64) -> Result<(), FileSystemError>;

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
    fn allocate_storage(&self, _offset: u64, _length: u64) -> Result<(), FileSystemError> {
        Err(FileSystemError::InvalidOperation)
    }

    fn sync_storage(&self) -> Result<(), FileSystemError>;

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
