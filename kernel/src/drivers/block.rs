use alloc::sync::Arc;
use spin::Mutex;

/// 启动块设备错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockError {
    InvalidBlock,
    IoError,
    DeviceError,
    AlreadyRegistered,
}

/// @description 为文件系统提供同步固定块读写与持久化屏障。
pub trait BlockDevice: Send + Sync {
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

    /// @description 写入一个完整逻辑块，返回前设备已消费 DMA buffer。
    ///
    /// @param block_id 从零开始的逻辑块号。
    /// @param buf 长度必须等于 `block_size()` 的源缓冲区。
    /// @return 成功时返回完整块字节数。
    /// @errors 块号越界、缓冲区长度错误或设备 I/O 失败时返回错误。
    fn write_block(&self, block_id: usize, buf: &[u8]) -> Result<usize, BlockError>;

    /// @description 把设备已接受的写入推进到稳定存储能力边界。
    ///
    /// @return flush 完成或设备明确不需要额外 flush 时返回成功。
    /// @errors 设备报告 I/O 或 unsupported 时返回错误。
    fn flush(&self) -> Result<(), BlockError>;

    /// 返回逻辑块字节数。
    fn block_size(&self) -> usize;
}

static PRIMARY_BLOCK_DEVICE: spin::Once<Mutex<Option<Arc<dyn BlockDevice>>>> = spin::Once::new();

fn primary_slot() -> &'static Mutex<Option<Arc<dyn BlockDevice>>> {
    PRIMARY_BLOCK_DEVICE.call_once(|| Mutex::new(None))
}

/// 注册唯一启动块设备。
pub fn register_block_device(device: Arc<dyn BlockDevice>) -> Result<usize, BlockError> {
    let mut slot = primary_slot().lock();
    if slot.is_some() {
        return Err(BlockError::AlreadyRegistered);
    }
    *slot = Some(device);
    Ok(0)
}

/// 取得唯一启动块设备。
pub fn get_primary_block_device() -> Option<Arc<dyn BlockDevice>> {
    primary_slot().lock().clone()
}

pub const BLOCK_SIZE: usize = 4096;
