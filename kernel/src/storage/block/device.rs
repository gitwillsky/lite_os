//! 块设备 trait：文件系统与分区视图依赖的同步固定块读写与持久化屏障。

/// 启动块设备错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BlockError {
    InvalidBlock,
    IoError,
    DeviceError,
    OutOfMemory,
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
