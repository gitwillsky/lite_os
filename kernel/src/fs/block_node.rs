//! 一块磁盘的 bdev 状态（Linux `block_device`）：字节寻址 I/O 与“已挂载/有写者”的互斥。
//!
//! 裸块设备经 page cache 缓冲（与 Linux bdev 一致），而 ext4 直接读写块层。两者同时修改同一块盘会
//! 互相覆盖，所以一块盘要么被某个文件系统挂载（此时节点只读、不经缓存），要么可以被打开写入，不能两者
//! 同时存在。

use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicUsize, Ordering};

use super::{
    FileSystemError,
    block_range::{self, BlockStore, RangeError},
    device::DeviceNumber,
};
use crate::drivers::block::{BlockDevice, BlockError};

/// `claim` 的最高位：设备已被文件系统挂载。其余位是以写方式打开的 OFD 数。
const MOUNTED: usize = 1 << (usize::BITS - 1);

/// 一块已发布的磁盘。
pub(crate) struct BlockNode {
    number: DeviceNumber,
    device: Arc<dyn BlockDevice>,
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
    ) -> Result<Arc<Self>, FileSystemError> {
        Arc::try_new(Self {
            number,
            device,
            claim: AtomicUsize::new(0),
        })
        .map_err(|_| FileSystemError::OutOfMemory)
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
