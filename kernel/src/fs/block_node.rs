//! 一块磁盘的 bdev 状态（Linux `block_device`）：字节寻址 I/O 与“已挂载/有写者”的互斥。
//!
//! 裸块设备经 page cache 缓冲（与 Linux bdev 一致），而 ext4 直接读写块层。两者同时修改同一块盘会
//! 互相覆盖，所以一块盘要么被某个文件系统挂载（此时节点只读、不经缓存），要么可以被打开写入，不能两者
//! 同时存在。

use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicUsize, Ordering};

use super::{
    CreateMetadata, DataBacking, DirectoryRead, DirectoryVisitor, FileSystemError, Inode,
    InodeMetadata, InodeType, OwnerModeChange,
    block_identity::PartUuid,
    block_range::{self, BlockStore, RangeError},
    device::{DeviceError, DeviceNumber, IoctlCall},
};
use crate::drivers::block::{BlockDevice, BlockError};

/// `claim` 的最高位：设备已被文件系统挂载。其余位是以写方式打开的 OFD 数。
const MOUNTED: usize = 1 << (usize::BITS - 1);

/// 分区在整盘中的位置与标识。
#[derive(Clone, Copy)]
pub(super) struct PartitionInfo {
    pub(super) start: u64,
    pub(super) uuid: Option<PartUuid>,
}

/// 一块已发布的磁盘。
pub(crate) struct BlockNode {
    number: DeviceNumber,
    device: Arc<dyn BlockDevice>,
    /// 整盘节点已发布的分区；分区节点为空。`BLKRRPART` 以它判断分区表是否变化。
    partitions: spin::Mutex<Vec<super::partition_table::Partition>>,
    /// 分区的位置与 `PARTUUID`；整盘为 `None`。
    partition: Option<PartitionInfo>,
    // OWNER: 挂载标志与写者计数的唯一原子字：`begin_writer` 与 `begin_mount` 对同一个字做 CAS，
    // 因此“挂载时还有写者”与“挂载后又出现写者”都不可能发生。放在原子里而不是锁里，是因为
    // `end_writer` 由 OFD 的 Drop 调用，不能阻塞。缺失时 mkfs 式写入与 ext4 会同时改同一块盘。
    claim: AtomicUsize,
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
        Arc::try_new(Self {
            number,
            device,
            partitions: spin::Mutex::new(Vec::new()),
            partition,
            claim: AtomicUsize::new(0),
        })
        .map_err(|_| FileSystemError::OutOfMemory)
    }

    pub(crate) fn is_partition(&self) -> bool {
        self.partition.is_some()
    }

    /// 分区的 `PARTUUID`；整盘与没有可用标识的分区为 `None`。
    pub(crate) fn partuuid(&self) -> Option<PartUuid> {
        self.partition.and_then(|partition| partition.uuid)
    }

    /// 分区在整盘中的起始扇区（512 字节）；整盘为 `None`。
    pub(crate) fn partition_start(&self) -> Option<u64> {
        self.partition.map(|partition| partition.start)
    }

    /// `/dev` 下的节点名（`vda`、`vda1`）。
    pub(crate) fn name(&self) -> &[u8] {
        self.device.disk_name()
    }

    /// 记录整盘已发布的分区集合。
    pub(super) fn set_partitions(&self, partitions: Vec<super::partition_table::Partition>) {
        *self.partitions.lock() = partitions;
    }

    /// 重新读取分区表并与已发布集合比较（`BLKRRPART`）。
    ///
    /// # Errors
    ///
    /// 分区表与已发布的不同返回 `Busy`：设备注册表只追加，不支持热移除或改号已发布的分区节点。
    pub(crate) fn reread_partitions(&self) -> Result<(), FileSystemError> {
        let current = super::partition_table::parse(self, self.capacity() / 512);
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

    /// 设备是否已被文件系统挂载。
    pub(crate) fn mounted(&self) -> bool {
        self.claim.load(Ordering::Acquire) & MOUNTED != 0
    }

    /// page cache 中这块盘的唯一身份：与 devtmpfs 实例无关，所以多个 devtmpfs 挂载共享同一份缓存。
    pub(crate) fn cache_id(&self) -> crate::memory::SharedFileId {
        crate::memory::SharedFileId {
            filesystem: super::BDEV_FILESYSTEM_ID,
            inode: u64::from(self.number.major) << 32 | u64::from(self.number.minor),
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
        block_range::read_range(self, offset, output, &mut scratch).map_err(block_error)
    }

    /// 向 `offset` 写；到设备末尾为止，起点越界返回 `NoSpace`。
    pub(crate) fn write(&self, offset: u64, input: &[u8]) -> Result<usize, FileSystemError> {
        let mut scratch = self.scratch()?;
        block_range::write_range(self, offset, input, &mut scratch).map_err(|error| match error {
            RangeError::Full => FileSystemError::NoSpace,
            RangeError::Store(error) => block_error(error),
        })
    }

    /// 把设备已接受的写入推进到稳定存储。
    pub(crate) fn flush(&self) -> Result<(), FileSystemError> {
        self.device.flush().map_err(block_error)
    }

    /// 登记一个以写方式打开的 OFD。
    ///
    /// # Errors
    ///
    /// 设备已被挂载返回 `Busy`。
    pub(crate) fn begin_writer(&self) -> Result<(), FileSystemError> {
        self.claim
            .try_update(Ordering::AcqRel, Ordering::Acquire, |claim| {
                (claim & MOUNTED == 0).then(|| claim + 1)
            })
            .map(|_| ())
            .map_err(|_| FileSystemError::Busy)
    }

    /// 撤销一次 [`Self::begin_writer`]；由 OFD 的 Drop 调用，不阻塞。
    pub(crate) fn end_writer(&self) {
        let previous = self.claim.fetch_sub(1, Ordering::AcqRel);
        assert_ne!(
            previous & !MOUNTED,
            0,
            "block writer released without acquire"
        );
    }

    /// 在文件系统读取这块盘之前声明挂载。
    ///
    /// 1. 没有写者时原子地置位挂载标志，此后新的写者被拒绝；
    /// 2. 写回并逐出 page cache 中这块盘的缓冲页，使文件系统直接读块层看到此前的裸写入；
    ///    仍被映射或使用的缓存无法安全逐出，撤销标志并返回 `Busy`。
    ///
    /// # Errors
    ///
    /// 已有写者、已挂载或缓存仍被使用返回 `Busy`；写回失败返回对应错误。
    pub(crate) fn begin_mount(&self) -> Result<(), FileSystemError> {
        self.claim
            .compare_exchange(0, MOUNTED, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| FileSystemError::Busy)?;
        if let Err(error) = super::page_cache::evict_cached(self.cache_id()) {
            self.claim.store(0, Ordering::Release);
            return Err(error);
        }
        Ok(())
    }

    /// 撤销挂载标志（卸载，或挂载在发布前失败）。
    pub(crate) fn end_mount(&self) {
        let previous = self.claim.swap(0, Ordering::AcqRel);
        assert_eq!(previous, MOUNTED, "mounted block device had writers");
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
    let Some(block) = inode.device_number().and_then(super::device::block_node) else {
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
        if self.block.mounted() {
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

    fn write_storage(&self, offset: u64, buf: &[u8]) -> Result<usize, FileSystemError> {
        self.block.write(offset, buf)
    }

    fn append_storage(&self, _buf: &[u8]) -> Result<(u64, usize), FileSystemError> {
        Err(FileSystemError::InvalidOperation)
    }

    fn truncate_storage(&self, _size: u64) -> Result<(), FileSystemError> {
        Err(FileSystemError::InvalidOperation)
    }

    fn sync_storage(&self) -> Result<(), FileSystemError> {
        self.block.flush()
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

impl super::partition_table::SectorSource for BlockNode {
    fn read(&self, lba: u64, buffer: &mut [u8]) -> bool {
        BlockNode::read(self, lba * 512, buffer).is_ok_and(|count| count == buffer.len())
    }
}
