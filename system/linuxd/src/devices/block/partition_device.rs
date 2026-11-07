//! 分区的块设备视图：整盘 `BlockDevice` 的一段连续块区间。

use alloc::{sync::Arc, vec::Vec};

use super::{BlockDevice, BlockError};
use device_model::{BlockBuffer, BlockOperation, BlockRequest};
use native_runtime::Vmo;

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

    fn translate(&self, block_id: usize, count: usize) -> Result<usize, BlockError> {
        if count == 0
            || block_id
                .checked_add(count)
                .is_none_or(|end| end > self.blocks)
        {
            return Err(BlockError::InvalidBlock);
        }
        self.first_block
            .checked_add(block_id)
            .ok_or(BlockError::InvalidBlock)
    }
}

impl BlockDevice for PartitionDevice {
    fn disk_name(&self) -> &[u8] {
        &self.name
    }

    fn attach_buffer(&self, vmo: Vmo) -> Result<Arc<dyn BlockBuffer>, BlockError> {
        self.parent.attach_buffer(vmo)
    }
    fn transfer(
        &self,
        operation: BlockOperation,
        first: u64,
        count: usize,
        buffer: &dyn BlockBuffer,
        offset: u64,
    ) -> Result<(), BlockError> {
        let first = self.translate(
            usize::try_from(first).map_err(|_| BlockError::InvalidBlock)?,
            count,
        )?;
        self.parent
            .transfer(operation, first as u64, count, buffer, offset)
    }
    fn transfer_batch(&self, requests: &[BlockRequest<'_>]) -> Result<(), BlockError> {
        let mut translated: Vec<BlockRequest<'_>> = Vec::new();
        translated
            .try_reserve_exact(requests.len())
            .map_err(|_| BlockError::OutOfMemory)?;
        let mut first_error = None;
        for request in requests {
            let first = usize::try_from(request.first_block)
                .map_err(|_| BlockError::InvalidBlock)
                .and_then(|first| self.translate(first, request.blocks));
            match first {
                Ok(first) => translated.push(BlockRequest {
                    first_block: first as u64,
                    ..*request
                }),
                Err(error) => {
                    first_error.get_or_insert(error);
                }
            }
        }
        if let Err(error) = self.parent.transfer_batch(&translated) {
            first_error.get_or_insert(error);
        }
        first_error.map_or(Ok(()), Err)
    }
    fn read_block_ranges(&self, ranges: &mut [(usize, &mut [u8])]) -> Result<(), BlockError> {
        let block_size = self.block_size();
        let mut translated: Vec<(usize, &mut [u8])> = Vec::new();
        translated
            .try_reserve_exact(ranges.len())
            .map_err(|_| BlockError::OutOfMemory)?;
        for (first, output) in ranges.iter_mut() {
            if !output.len().is_multiple_of(block_size) {
                return Err(BlockError::InvalidBlock);
            }
            translated.push((
                self.translate(*first, output.len() / block_size)?,
                &mut **output,
            ));
        }
        self.parent.read_block_ranges(&mut translated)
    }
    fn max_transfer_blocks(&self) -> usize {
        self.parent.max_transfer_blocks()
    }
    fn queue_depth(&self) -> usize {
        self.parent.queue_depth()
    }

    fn read_blocks(&self, block_id: usize, buf: &mut [u8]) -> Result<usize, BlockError> {
        if !buf.len().is_multiple_of(self.block_size()) {
            return Err(BlockError::InvalidBlock);
        }
        self.parent.read_blocks(
            self.translate(block_id, buf.len() / self.block_size())?,
            buf,
        )
    }

    fn write_blocks(&self, block_id: usize, buf: &[u8]) -> Result<usize, BlockError> {
        if !buf.len().is_multiple_of(self.block_size()) {
            return Err(BlockError::InvalidBlock);
        }
        self.parent.write_blocks(
            self.translate(block_id, buf.len() / self.block_size())?,
            buf,
        )
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

    /// 分区内区间平移到整盘块号后由整盘共享；越出分区返回 `None`。
    fn share_range(&self, first_block: u64, block_count: u64) -> Option<native_runtime::Handle> {
        let end = first_block.checked_add(block_count)?;
        if block_count == 0 || end > self.blocks as u64 {
            return None;
        }
        self.parent.share_range(
            (self.first_block as u64).checked_add(first_block)?,
            block_count,
        )
    }
}
