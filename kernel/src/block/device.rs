//! 块设备 trait：文件系统与分区视图依赖的同步固定块读写与持久化屏障，以及不阻塞调用者的异步读。

use alloc::{boxed::Box, sync::Arc, vec::Vec};

/// 启动块设备错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BlockError {
    InvalidBlock,
    IoError,
    DeviceError,
    OutOfMemory,
    /// 异步提交暂时没有空闲请求槽；调用者稍后重试或改走同步路径。
    WouldBlock,
}

/// 异步读完成时被调用的通知。实现必须可以在 deferred completion 上下文里调用（不睡眠、不分配）。
pub(crate) trait CompletionNotify: Send + Sync {
    fn notify(&self);
}

/// 已提交、尚未取走结果的块读。
///
/// 丢弃一个未完成的句柄表示取消：设备仍会完成这个请求，但结果被丢弃，请求槽由完成路径自己释放；
/// 缺失该约定，被放弃的读会永久占用请求槽。
pub(crate) trait PendingRead: Send {
    /// 请求是否已完成（成功或失败）；完成后 [`PendingRead::finish`] 不会阻塞。
    fn is_complete(&self) -> bool;

    /// 取走结果并把读到的块复制到 `output`（长度必须等于块大小）。只能在完成后调用。
    fn finish(self: Box<Self>, output: &mut [u8]) -> Result<(), BlockError>;
}

/// 在提交时就已读完的块（没有异步能力的设备使用）。
struct ReadyRead(Vec<u8>);

impl PendingRead for ReadyRead {
    fn is_complete(&self) -> bool {
        true
    }

    fn finish(self: Box<Self>, output: &mut [u8]) -> Result<(), BlockError> {
        output.copy_from_slice(&self.0);
        Ok(())
    }
}

/// 为文件系统提供同步固定块读写与持久化屏障。
pub(crate) trait BlockDevice: Send + Sync {
    /// 内核分配的磁盘名（不含 `/dev/`），例如 `vda`；adapter 的整个生命周期内不变。
    fn disk_name(&self) -> &[u8];

    /// 读取一个完整逻辑块。
    ///
    /// # Parameters
    ///
    /// - `block_id`: 从零开始的逻辑块号。
    /// - `buf`: 长度必须等于 `block_size()` 的目标缓冲区。
    ///
    /// # Returns
    ///
    /// 成功时返回完整块字节数。
    ///
    /// # Errors
    ///
    /// 块号越界、缓冲区长度错误或设备 I/O 失败时返回错误。
    fn read_block(&self, block_id: usize, buf: &mut [u8]) -> Result<usize, BlockError>;

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
    fn write_block(&self, block_id: usize, buf: &[u8]) -> Result<usize, BlockError>;

    /// 提交一个整块读但不等待，完成时调用 `notify`。
    ///
    /// 默认实现同步读完再返回已完成的句柄，适用于没有异步能力的设备（它们不会调用 `notify`）；
    /// 具备异步能力的驱动必须覆盖它，否则调用者会在提交处睡眠。
    ///
    /// # Returns
    ///
    /// 可轮询完成的句柄。
    ///
    /// # Errors
    ///
    /// 块号越界、设备失败或内存不足；没有空闲请求槽时返回 `WouldBlock`。
    fn begin_read(
        self: Arc<Self>,
        block_id: usize,
        notify: Arc<dyn CompletionNotify>,
    ) -> Result<Box<dyn PendingRead>, BlockError> {
        let _ = notify;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(self.block_size())
            .map_err(|_| BlockError::OutOfMemory)?;
        bytes.resize(self.block_size(), 0);
        self.read_block(block_id, &mut bytes)?;
        Ok(Box::new(ReadyRead(bytes)))
    }

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
}

pub(crate) const BLOCK_SIZE: usize = 4096;
