//! bdev 的字节寻址 I/O 与物理 extent claim；整盘和分区共享唯一 claim owner。
//! 挂载/写打开先拒绝重叠 writer/mount，并在锁外写回/逐出对应 bdev alias cache。

use alloc::{sync::Arc, vec::Vec};

use super::{
    CreateMetadata, DataBacking, DirectoryRead, DirectoryVisitor, FileSystemError, Inode,
    InodeMetadata, InodeType, OwnerModeChange,
    block_claim::BlockClaim,
    {
        DeviceNumber,
        chardev::{DeviceError, IoctlCall},
    },
};
use crate::devices::block::{
    BlockDevice, BlockError,
    identity::PartUuid,
    range::{self, BlockStore, RangeError},
};

/// 分区在整盘中的位置与标识。
#[derive(Clone)]
pub(super) struct PartitionInfo {
    pub(super) whole: Arc<BlockNode>,
    pub(super) start: u64,
    pub(super) uuid: Option<PartUuid>,
}

/// 一块已发布的磁盘。
pub(crate) struct BlockNode {
    number: DeviceNumber,
    device: Arc<dyn BlockDevice>,
    /// 整盘节点已发布的分区；分区节点为空。`BLKRRPART` 以它判断分区表是否变化。
    partitions: spin::Mutex<Vec<crate::devices::block::partition_table::Partition>>,
    /// 分区的位置与 `PARTUUID`；整盘为 `None`。
    partition: Option<PartitionInfo>,
    /// physical extent 的唯一盘 claim owner 与稳定节点 key。
    claim: BlockClaim,
}

fn device_key(number: DeviceNumber) -> u64 {
    u64::from(number.major) << 32 | u64::from(number.minor)
}

fn block_error(error: BlockError) -> FileSystemError {
    match error {
        BlockError::OutOfMemory => FileSystemError::OutOfMemory,
        _ => FileSystemError::IoError,
    }
}

impl BlockStore for BlockNode {
    type Error = BlockError;

    fn block_size(&self) -> usize {
        self.device.block_size()
    }

    fn block_count(&self) -> u64 {
        self.device.block_count()
    }

    fn read_block(&self, block: usize, buffer: &mut [u8]) -> Result<(), BlockError> {
        self.device.read_block(block, buffer).map(|_| ())
    }

    fn write_block(&self, block: usize, buffer: &[u8]) -> Result<(), BlockError> {
        self.device.write_block(block, buffer).map(|_| ())
    }
}

impl BlockNode {
    pub(super) fn new(
        number: DeviceNumber,
        device: Arc<dyn BlockDevice>,
        partition: Option<PartitionInfo>,
    ) -> Result<Arc<Self>, FileSystemError> {
        let bytes = device
            .block_count()
            .checked_mul(device.block_size() as u64)
            .ok_or(FileSystemError::InvalidOperation)?;
        let claim = match &partition {
            Some(partition) => partition.whole.claim.partition(
                device_key(number),
                partition
                    .start
                    .checked_mul(512)
                    .ok_or(FileSystemError::InvalidOperation)?,
                bytes,
            )?,
            None => BlockClaim::whole(device_key(number), bytes)?,
        };
        Arc::try_new(Self {
            number,
            device,
            partitions: spin::Mutex::new(Vec::new()),
            partition,
            claim,
        })
        .map_err(|_| FileSystemError::OutOfMemory)
    }

    pub(crate) fn is_partition(&self) -> bool {
        self.partition.is_some()
    }

    /// 分区的 `PARTUUID`；整盘与没有可用标识的分区为 `None`。
    pub(crate) fn partuuid(&self) -> Option<PartUuid> {
        self.partition.as_ref().and_then(|partition| partition.uuid)
    }

    /// 分区在整盘中的起始扇区（512 字节）；整盘为 `None`。
    pub(crate) fn partition_start(&self) -> Option<u64> {
        self.partition.as_ref().map(|partition| partition.start)
    }

    /// `/dev` 下的节点名（`vda`、`vda1`）。
    pub(crate) fn name(&self) -> &[u8] {
        self.device.disk_name()
    }

    /// 记录整盘已发布的分区集合。
    pub(super) fn set_partitions(
        &self,
        partitions: Vec<crate::devices::block::partition_table::Partition>,
    ) {
        *self.partitions.lock() = partitions;
    }

    /// 重新读取分区表并与已发布集合比较（`BLKRRPART`）。
    ///
    /// # Errors
    ///
    /// 分区表与已发布的不同返回 `Busy`：设备注册表只追加，不支持热移除或改号已发布的分区节点。
    pub(crate) fn reread_partitions(&self) -> Result<(), FileSystemError> {
        let current = crate::devices::block::partition_table::parse(self, self.capacity() / 512);
        let current = super::publishable_partitions(current);
        if *self.partitions.lock() == current {
            Ok(())
        } else {
            Err(FileSystemError::Busy)
        }
    }

    pub(crate) fn number(&self) -> DeviceNumber {
        self.number
    }

    pub(crate) fn device(&self) -> &Arc<dyn BlockDevice> {
        &self.device
    }

    /// 设备总字节数。
    pub(crate) fn capacity(&self) -> u64 {
        self.device
            .block_count()
            .saturating_mul(self.device.block_size() as u64)
    }

    /// 物理 claim owner 投影：挂载或其他重叠 alias writer 时禁止新建独立 cache。
    pub(crate) fn bypass_cache(&self) -> bool {
        self.claim.bypass_cache()
    }

    /// page cache 中这块盘的唯一身份：与 devtmpfs 实例无关，所以多个 devtmpfs 挂载共享同一份缓存。
    pub(crate) fn cache_id(&self) -> crate::memory::SharedFileId {
        crate::memory::SharedFileId {
            filesystem: super::BDEV_FILESYSTEM_ID,
            inode: device_key(self.number),
        }
    }

    fn scratch(&self) -> Result<Vec<u8>, FileSystemError> {
        let size = self.device.block_size();
        let mut scratch = Vec::new();
        scratch
            .try_reserve_exact(size)
            .map_err(|_| FileSystemError::OutOfMemory)?;
        scratch.resize(size, 0);
        Ok(scratch)
    }

    /// 从 `offset` 读；到设备末尾为止。
    pub(crate) fn read(&self, offset: u64, output: &mut [u8]) -> Result<usize, FileSystemError> {
        let mut scratch = self.scratch()?;
        range::read_range(self, offset, output, &mut scratch).map_err(block_error)
    }

    /// 向 `offset` 写；到设备末尾为止，起点越界返回 `NoSpace`。
    pub(crate) fn write(&self, offset: u64, input: &[u8]) -> Result<usize, FileSystemError> {
        let mut scratch = self.scratch()?;
        range::write_range(self, offset, input, &mut scratch).map_err(|error| match error {
            RangeError::Full => FileSystemError::NoSpace,
            RangeError::Store(error) => block_error(error),
        })
    }

    /// 把设备已接受的写入推进到稳定存储。
    pub(crate) fn flush(&self) -> Result<(), FileSystemError> {
        self.device.flush().map_err(block_error)
    }

    /// 逐出重叠 bdev alias 的缓存；claim 已先发布，所有 I/O 都在 claim lock 外。
    fn evict_aliases(&self, include_self: bool) -> Result<(), FileSystemError> {
        let mut after = None;
        while let Some(key) = self.claim.next_alias(after) {
            after = Some(key);
            if include_self || key != device_key(self.number) {
                super::page_cache::evict_cached(crate::memory::SharedFileId {
                    filesystem: super::BDEV_FILESYSTEM_ID,
                    inode: key,
                })?;
            }
        }
        Ok(())
    }

    /// Swap 与 mount/raw writer 共用排他物理 extent owner；alias cache retirement 失败回滚。
    pub(super) fn begin_swap(&self) -> Result<(), FileSystemError> {
        self.claim.begin_swap()?;
        if let Err(error) = self.evict_aliases(true) {
            self.claim.end_swap();
            return Err(error);
        }
        Ok(())
    }

    pub(super) fn end_swap(&self) {
        self.claim.end_swap();
    }

    /// 登记写 OFD，拒绝任何重叠 mount 或其他 bdev writer；不能安全逐出 alias cache 时回滚。
    pub(crate) fn begin_writer(&self) -> Result<(), FileSystemError> {
        self.claim.begin_writer()?;
        if let Err(error) = self.evict_aliases(false) {
            self.claim.end_writer();
            return Err(error);
        }
        Ok(())
    }

    /// OFD Drop 撤销一次写打开，不分配、不 I/O。
    pub(crate) fn end_writer(&self) {
        self.claim.end_writer();
    }

    /// 先领取物理 extent，再写回/逐出整盘和所有重叠分区的缓存，最后才允许 filesystem I/O。
    /// 任一 alias 仍被映射或写回失败时回滚 claim 并返回原错误。
    pub(crate) fn begin_mount(&self) -> Result<(), FileSystemError> {
        self.claim.begin_mount()?;
        if let Err(error) = self.evict_aliases(true) {
            self.claim.end_mount();
            return Err(error);
        }
        Ok(())
    }

    /// 撤销挂载 claim（卸载，或挂载在发布前失败）。
    pub(crate) fn end_mount(&self) {
        self.claim.end_mount();
    }
}

/// 路径上的块设备节点（任何文件系统里的 `S_IFBLK` inode）在 VFS 里的唯一视图。
///
/// 节点本身只记录“这是设备 `N`”，内容在设备上而不在节点里。VFS 在发布 opened entry 时把块设备节点
/// 包装成它：metadata、权限与 chmod/chown 仍由节点所属文件系统负责，字节 I/O、容量、page cache 身份、
/// ioctl 与 fsync 则统一落到 [`BlockNode`]。缺失这层时，每个文件系统的块设备节点都得各自实现一遍
/// 设备 I/O，并且 tmpfs/ext4 上的节点读写会落在节点自己（没有数据）而不是设备上。
pub(crate) struct BlockSpecial {
    inner: Arc<dyn Inode>,
    block: Arc<BlockNode>,
}

/// 把块设备节点包装成 [`BlockSpecial`]；其他 inode、或节点指向不存在的盘时原样返回。
pub(super) fn wrap_block_node(inode: Arc<dyn Inode>) -> Result<Arc<dyn Inode>, FileSystemError> {
    if inode.inode_type() != InodeType::BlockDevice {
        return Ok(inode);
    }
    let Some(block) = inode.device_number().and_then(super::devnode::block_node) else {
        return Ok(inode);
    };
    Arc::try_new(BlockSpecial {
        inner: inode,
        block,
    })
    .map(|wrapped| wrapped as Arc<dyn Inode>)
    .map_err(|_| FileSystemError::OutOfMemory)
}

impl Inode for BlockSpecial {
    fn filesystem_id(&self) -> usize {
        self.inner.filesystem_id()
    }

    fn notification_link_state(&self) -> Option<(u64, u32)> {
        self.inner.notification_link_state()
    }

    fn metadata(&self) -> Result<InodeMetadata, FileSystemError> {
        self.inner.metadata()
    }

    fn inode_type(&self) -> InodeType {
        InodeType::BlockDevice
    }

    /// 设备容量（`lseek(SEEK_END)` 与 page cache 的 EOF）；`st_size` 仍是节点的 0。
    fn size(&self) -> u64 {
        self.block.capacity()
    }

    fn is_executable(&self) -> bool {
        false
    }

    fn device_number(&self) -> Option<DeviceNumber> {
        Some(self.block.number())
    }

    /// 写设备不是修改文件系统：只读挂载上的块设备节点仍可按权限打开写入。
    fn is_read_only(&self) -> bool {
        false
    }

    fn page_cache_id(&self) -> Result<crate::memory::SharedFileId, FileSystemError> {
        Ok(self.block.cache_id())
    }

    /// 已挂载的块设备由文件系统直接读写块层，节点退化为只读、不缓冲的视图（见 [`BlockNode`]）。
    fn data_backing(&self) -> DataBacking {
        if self.block.bypass_cache() {
            DataBacking::Snapshot
        } else {
            DataBacking::PageCache
        }
    }

    fn ioctl(&self, call: &IoctlCall<'_>) -> Result<isize, DeviceError> {
        super::block_ioctl::ioctl(&self.block, call)
    }

    fn read_storage(&self, offset: u64, buf: &mut [u8]) -> Result<usize, FileSystemError> {
        self.block.read(offset, buf)
    }

    fn write_storage(
        &self,
        offset: u64,
        buf: &[u8],
        _identity: Option<&crate::fs::AccessIdentity>,
    ) -> Result<usize, FileSystemError> {
        self.block.write(offset, buf)
    }

    fn append_storage(
        &self,
        _buf: &[u8],
        _identity: Option<&crate::fs::AccessIdentity>,
    ) -> Result<(u64, usize), FileSystemError> {
        Err(FileSystemError::InvalidOperation)
    }

    fn truncate_storage(
        &self,
        _size: u64,
        _identity: Option<&crate::fs::AccessIdentity>,
    ) -> Result<(), FileSystemError> {
        Err(FileSystemError::InvalidOperation)
    }

    fn sync_storage(&self) -> Result<(), FileSystemError> {
        self.block.flush()
    }

    fn write_access(&self) -> Option<&super::InodeWriteAccess> {
        self.inner.write_access()
    }
    fn touch_access(&self, now: u64, relatime: bool) -> Result<(), FileSystemError> {
        self.inner.touch_access(now, relatime)
    }
    fn set_times(&self, atime: Option<u64>, mtime: Option<u64>) -> Result<(), FileSystemError> {
        self.inner.set_times(atime, mtime)
    }

    fn change_owner_mode(&self, change: OwnerModeChange) -> Result<(), FileSystemError> {
        self.inner.change_owner_mode(change)
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

impl crate::devices::block::partition_table::SectorSource for BlockNode {
    fn read(&self, lba: u64, buffer: &mut [u8]) -> bool {
        BlockNode::read(self, lba * 512, buffer).is_ok_and(|count| count == buffer.len())
    }
}
