use core::any::Any;
use alloc::sync::Arc;
use alloc::string::String;
use alloc::vec::Vec;
use alloc::format;
use alloc::vec;

use super::hal::{
    Device, DeviceType, DeviceState, DeviceError, 
    device::DeviceDriver,
    Bus, InterruptVector, InterruptHandler,
    resource::{Resource, ResourceManager}
};

/// 块设备错误类型
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockError {
    InvalidBlock,
    IoError,
    DeviceError,
    OutOfMemory,
    NotSupported,
}

impl From<DeviceError> for BlockError {
    fn from(error: DeviceError) -> Self {
        match error {
            DeviceError::NotSupported => BlockError::NotSupported,
            DeviceError::HardwareError => BlockError::DeviceError,
            DeviceError::OperationFailed => BlockError::IoError,
            DeviceError::TimeoutError => BlockError::IoError,
            _ => BlockError::DeviceError,
        }
    }
}

/// 块设备特性 - 现在基于HAL Device，使用内部可变性
pub trait BlockDevice: Device + Send + Sync {
    /// 读取块数据
    fn read_block(&self, block_id: usize, buf: &mut [u8]) -> Result<usize, BlockError>;
    
    /// 写入块数据
    fn write_block(&self, block_id: usize, buf: &[u8]) -> Result<usize, BlockError>;
    
    /// 获取块数量
    fn num_blocks(&self) -> usize;
    
    /// 获取块大小
    fn block_size(&self) -> usize;
    
    /// 异步读取块（可选实现）
    fn read_block_async(&self, _block_id: usize, _buf: &mut [u8]) -> Result<(), BlockError> {
        Err(BlockError::NotSupported)
    }
    
    /// 异步写入块（可选实现）
    fn write_block_async(&self, _block_id: usize, _buf: &[u8]) -> Result<(), BlockError> {
        Err(BlockError::NotSupported)
    }
    
    /// 同步所有挂起的写入操作
    fn sync(&self) -> Result<(), BlockError> {
        Ok(()) // 默认实现为无操作
    }
    
    /// 获取设备统计信息
    fn statistics(&self) -> BlockDeviceStats {
        BlockDeviceStats::default()
    }
}

/// 块设备统计信息
#[derive(Debug, Clone, Default)]
pub struct BlockDeviceStats {
    pub read_count: u64,
    pub write_count: u64,
    pub bytes_read: u64,
    pub bytes_written: u64,
    pub read_errors: u64,
    pub write_errors: u64,
}

/// 通用块设备驱动程序
pub struct GenericBlockDriver {
    name: &'static str,
    version: &'static str,
}

impl GenericBlockDriver {
    pub fn new() -> Self {
        Self {
            name: "Generic Block Driver",
            version: "1.0.0",
        }
    }
}

impl DeviceDriver for GenericBlockDriver {
    fn name(&self) -> &str {
        self.name
    }
    
    fn version(&self) -> &str {
        self.version
    }
    
    fn compatible_devices(&self) -> Vec<(u32, u32)> {
        // 支持通用块设备
        vec![(0x1af4, 0x1001)] // VirtIO block device
    }
    
    fn probe(&self, device: &mut dyn Device) -> Result<bool, DeviceError> {
        // 检查是否为块设备
        Ok(matches!(device.device_type(), DeviceType::Block))
    }
    
    fn bind(&self, device: &mut dyn Device) -> Result<(), DeviceError> {
        debug!("[BlockDriver] Binding to device: {}", device.device_name());
        
        // 设备初始化
        device.initialize()?;
        
        debug!("[BlockDriver] Successfully bound to device: {}", device.device_name());
        Ok(())
    }
    
    fn unbind(&self, device: &mut dyn Device) -> Result<(), DeviceError> {
        debug!("[BlockDriver] Unbinding from device: {}", device.device_name());
        
        // 执行清理操作
        device.shutdown()?;
        
        Ok(())
    }
    
    fn supports_hotplug(&self) -> bool {
        true
    }
}

/// 块设备管理器
pub struct BlockDeviceManager {
    devices: Vec<Arc<dyn BlockDevice>>,
}

impl BlockDeviceManager {
    pub fn new() -> Self {
        Self {
            devices: Vec::new(),
        }
    }
    
    /// 注册块设备
    pub fn register_device(&mut self, device: Arc<dyn BlockDevice>) -> Result<usize, BlockError> {
        let device_id = self.devices.len();
        self.devices.push(device);
        
        info!("[BlockManager] Registered block device #{}", device_id);
        Ok(device_id)
    }
    
    /// 获取块设备
    pub fn get_device(&self, device_id: usize) -> Option<Arc<dyn BlockDevice>> {
        self.devices.get(device_id).cloned()
    }
    
    /// 获取所有块设备
    pub fn get_all_devices(&self) -> Vec<Arc<dyn BlockDevice>> {
        self.devices.clone()
    }
    
    /// 查找第一个可用的块设备
    pub fn get_primary_device(&self) -> Option<Arc<dyn BlockDevice>> {
        self.devices.first().cloned()
    }
    
    /// 获取设备数量
    pub fn device_count(&self) -> usize {
        self.devices.len()
    }
}

/// 全局块设备管理器实例
static BLOCK_MANAGER: spin::Once<spin::Mutex<BlockDeviceManager>> = spin::Once::new();

/// 获取全局块设备管理器
pub fn block_manager() -> &'static spin::Mutex<BlockDeviceManager> {
    BLOCK_MANAGER.call_once(|| spin::Mutex::new(BlockDeviceManager::new()))
}

/// 注册块设备到全局管理器
pub fn register_block_device(device: Arc<dyn BlockDevice>) -> Result<usize, BlockError> {
    block_manager().lock().register_device(device)
}

/// 获取主要块设备（用于文件系统）
pub fn get_primary_block_device() -> Option<Arc<dyn BlockDevice>> {
    block_manager().lock().get_primary_device()
}

/// 获取所有块设备
pub fn get_all_block_devices() -> Vec<Arc<dyn BlockDevice>> {
    block_manager().lock().get_all_devices()
}

pub const BLOCK_SIZE: usize = 4096;