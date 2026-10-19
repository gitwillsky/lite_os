//! 分区的块设备视图：整盘 `BlockDevice` 的一段连续块区间。

use alloc::{sync::Arc, vec::Vec};

use super::{BlockDevice, BlockError};

/// 整盘中 `[first_block, first_block + blocks)` 的视图；块号在分区内从零起算。
///
/// 起点与长度按设备逻辑块（4 KiB）对齐，所以分区上的文件系统与整盘使用同一种整块读写，不需要
/// 字节级的 read-modify-write。未对齐的分区不会被发布（见 `publish_partitions`）。
pub(crate) struct PartitionDevice {
    name: Vec<u8>,
    parent: Arc<dyn BlockDevice>,
    first_block: usize,
    blocks: usize,
}

impl PartitionDevice {
    pub(crate) fn new(
        name: Vec<u8>,
        parent: Arc<dyn BlockDevice>,
        first_block: usize,
        blocks: usize,
    ) -> Self {
        Self {
            name,
            parent,
            first_block,
            blocks,
        }
    }

    fn translate(&self, block_id: usize) -> Result<usize, BlockError> {
        if block_id >= self.blocks {
            return Err(BlockError::InvalidBlock);
        }
        Ok(self.first_block + block_id)
    }
}

impl BlockDevice for PartitionDevice {
    fn disk_name(&self) -> &[u8] {
        &self.name
    }

    fn read_block(&self, block_id: usize, buf: &mut [u8]) -> Result<usize, BlockError> {
        self.parent.read_block(self.translate(block_id)?, buf)
    }

    fn write_block(&self, block_id: usize, buf: &[u8]) -> Result<usize, BlockError> {
        self.parent.write_block(self.translate(block_id)?, buf)
    }

    fn flush(&self) -> Result<(), BlockError> {
        self.parent.flush()
    }

    fn block_size(&self) -> usize {
        self.parent.block_size()
    }

    fn block_count(&self) -> u64 {
        self.blocks as u64
    }
}
