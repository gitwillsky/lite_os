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

/// @description 为只读启动文件系统提供固定块读取。
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
