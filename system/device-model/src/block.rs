//! 块设备：登记 VMO 区间传输、未登记 CPU 缓冲适配与持久化屏障。

use alloc::sync::Arc;
use core::any::Any;
use native_runtime::Vmo;

mod byte_buffers;
pub use byte_buffers::BlockBufferPool;

/// 一个数据传输请求的设备方向。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockOperation {
    /// 设备写入缓冲。
    Read,
    /// 设备读取缓冲。
    Write,
}

/// 批量提交中的一个传输：与 [`BlockDevice::transfer`] 的参数一一对应。
#[derive(Clone, Copy)]
pub struct BlockRequest<'a> {
    /// 相对设备的读写方向。
    pub operation: BlockOperation,
    /// 设备块区间起点。
    pub first_block: u64,
    /// 块数；非空且不超过协商上限。
    pub blocks: usize,
    /// 本设备登记的缓冲。
    pub buffer: &'a dyn BlockBuffer,
    /// 缓冲内按逻辑块对齐的字节偏移。
    pub buffer_offset: u64,
}

/// 由实际设备提供者登记并拥有的缓冲；不能用于另一个设备或会话。
pub trait BlockBuffer: Any + Send + Sync {
    /// 原始内容 VMO；调用者在传输完成之前不得修改或回收对应区间。
    fn vmo(&self) -> &Vmo;
}

/// 启动块设备错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockError {
    InvalidBlock,
    IoError,
    DeviceError,
    OutOfMemory,
}

/// 为文件系统提供同步固定块读写与持久化屏障。
pub trait BlockDevice: Send + Sync {
    /// 内核分配的磁盘名（不含 `/dev/`），例如 `vda`；adapter 的整个生命周期内不变。
    fn disk_name(&self) -> &[u8];

    /// 登记可读写 VMO；对象在会话存活期间保活登记，最后引用或会话关闭时回收。
    ///
    /// # Errors
    ///
    /// VMO 权限、大小或 DMA 固定失败；分配失败返回 OutOfMemory。
    fn attach_buffer(&self, vmo: Vmo) -> Result<Arc<dyn BlockBuffer>, BlockError>;

    /// 直接在登记 VMO 的 `buffer_offset` 字节处传输 `blocks` 个逻辑块，成功返回前 DMA 已完成。
    ///
    /// # Parameters
    ///
    /// - `operation`: 相对设备的读写方向。
    /// - `first_block`/`blocks`: 设备块区间；非空且不超过协商上限。
    /// - `buffer`/`buffer_offset`: 本设备登记的缓冲及按逻辑块对齐的字节偏移。
    ///
    /// # Errors
    ///
    /// 任一设备/缓冲边界、方向或登记身份无效，或设备 I/O 失败。失联后不得复用该登记：
    /// 目标内容不可用，旧 DMA 页由隔离机制保活直到设备复位；重连后重新登记才能恢复使用。
    fn transfer(
        &self,
        operation: BlockOperation,
        first_block: u64,
        blocks: usize,
        buffer: &dyn BlockBuffer,
        buffer_offset: u64,
    ) -> Result<(), BlockError>;

    /// 并发提交一批传输，全部完成后返回（Fuchsia block FIFO / Linux blk-mq 的批量提交语义）。
    ///
    /// 提供者可让整批同时占用设备队列，使单个调用者就能把队列填满；默认实���逐个同步执行。
    ///
    /// # Errors
    ///
    /// 任一请求无效或 I/O 失败。出错时其余请求仍会执行完毕（缓冲不能在设备仍可能写入时交还），
    /// 返回第一个错误；失败请求的目标内容不可用。
    fn transfer_batch(&self, requests: &[BlockRequest<'_>]) -> Result<(), BlockError> {
        let mut first_error = None;
        for request in requests {
            if let Err(error) = self.transfer(
                request.operation,
                request.first_block,
                request.blocks,
                request.buffer,
                request.buffer_offset,
            ) {
                first_error.get_or_insert(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    /// 把多个不相邻的完整块区间读入各自的未登记 CPU 缓冲，设备可并发处理整批。
    ///
    /// # Parameters
    ///
    /// - `ranges`: `(起始逻辑块, 目标缓冲)`；每个缓冲非空且长度为 `block_size()` 的整数倍。
    ///
    /// # Errors
    ///
    /// 任一区间越界、缓冲长度错误或设备 I/O 失败；出错时各缓冲内容不确定。
    fn read_block_ranges(&self, ranges: &mut [(usize, &mut [u8])]) -> Result<(), BlockError> {
        for (first, output) in ranges.iter_mut() {
            self.read_blocks(*first, output)?;
        }
        Ok(())
    }

    /// 单次传输保证可容纳的最大逻辑块数。
    fn max_transfer_blocks(&self) -> usize;

    /// 设备可同时拥有的请求数；用于会话准入及未登记 CPU 缓冲的有界复用。
    fn queue_depth(&self) -> usize;

    /// 把完整逻辑块区间读入未登记的 CPU 缓冲；原生提供者经同一 VMO 数据面适配。
    ///
    /// # Parameters
    ///
    /// - `block_id`: 从零开始的逻辑块号。
    /// - `buf`: 非空、长度为 `block_size()` 整数倍的目标缓冲区。
    ///
    /// # Returns
    ///
    /// 成功时返回完整块字节数。
    ///
    /// # Errors
    ///
    /// 块号越界、缓冲区长度错误或设备 I/O 失败时返回错误。
    fn read_blocks(&self, block_id: usize, buf: &mut [u8]) -> Result<usize, BlockError>;

    /// 写入一个完整逻辑块，返回前设备已消费 DMA buffer。
    ///
    /// # Parameters
    ///
    /// - `block_id`: 从零开始的逻辑块号。
    /// - `buf`: 长度必须等于 `block_size()` 的源缓冲区。
    ///
    /// # Returns
    ///
    /// 成功时返回完整块字节数。
    ///
    /// # Errors
    ///
    /// 块号越界、缓冲区长度错误或设备 I/O 失败时返回错误。
    fn write_blocks(&self, block_id: usize, buf: &[u8]) -> Result<usize, BlockError>;

    /// 把设备已接受的写入推进到稳定存储能力边界。
    ///
    /// # Returns
    ///
    /// flush 完成或设备明确不需要额外 flush 时返回成功。
    ///
    /// # Errors
    ///
    /// 设备报告 I/O 或 unsupported 时返回错误。
    fn flush(&self) -> Result<(), BlockError>;

    /// 返回逻辑块字节数。
    fn block_size(&self) -> usize;

    /// 返回设备的逻辑块总数（`block_size()` 单位）；裸块设备的容量与越界判定以它为准。
    fn block_count(&self) -> u64;

    /// 把本设备的 `[first_block, first_block + block_count)`（本设备块号）交给另一服务直接访问：
    /// 返回一条服务该范围的块协议连接。不能跨进程共享的设备返回 `None`。
    fn share_range(&self, first_block: u64, block_count: u64) -> Option<native_runtime::Handle> {
        let _ = (first_block, block_count);
        None
    }
}

pub const BLOCK_SIZE: usize = 4096;
