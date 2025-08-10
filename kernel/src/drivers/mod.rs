// LiteOS 驱动子系统 - 完全基于新HAL架构

use alloc::vec;
use alloc::string::String;

pub mod hal;                    // 硬件抽象层 - 核心架构
pub mod block;                  // 块设备抽象层
pub mod device_manager;         // 设备管理器 - 统一设备管理
pub mod virtio_blk;            // VirtIO块设备驱动
pub mod virtio_console;        // VirtIO控制台驱动
pub mod virtio_gpu;            // VirtIO GPU设备驱动
pub mod virtio_queue;          // VirtIO队列实现
pub mod framebuffer;           // Framebuffer抽象层
pub mod goldfish_rtc;          // Goldfish RTC驱动

// === HAL核心导出 ===
pub use hal::{
    // 设备抽象
    Device, DeviceType, DeviceState, DeviceError, GenericDevice,
    device::DeviceDriver,
    // 总线抽象
    Bus, BusError,
    // 中断处理
    InterruptHandler, InterruptController, InterruptVector,
    // 内存管理
    DmaBuffer, MemoryAttributes,
    // 电源管理
    PowerManagement, PowerState,
    // 资源管理
    Resource, ResourceManager,
    // 设备管理器
    DeviceManager,
};

// === 块设备子系统导出 ===
pub use block::{
    BlockDevice, BlockError, BlockDeviceStats,
    GenericBlockDriver,
    // 全局管理函数
    register_block_device, get_primary_block_device, get_all_block_devices,
    block_manager,
    BLOCK_SIZE,
};

// === 设备管理器导出 ===
pub use device_manager::{
    // 系统初始化
    init,
    // 中断处理
    handle_external_interrupt,
    // 电源管理
    suspend_all_devices, resume_all_devices,
    // 设备查找
    find_devices_by_type, find_devices_by_driver, get_device,
    // 统计和调试
    get_device_statistics,
    // 全局设备管理器
    device_manager,
};

// === VirtIO驱动导出 ===
pub use virtio_blk::VirtIOBlockDevice;
pub use virtio_console::{
    VirtIOConsoleDevice,
    // 全局控制台接口
    init_virtio_console,
    virtio_console_write,
    virtio_console_read,
    virtio_console_has_input,
    is_virtio_console_available,
};
pub use virtio_gpu::{VirtioGpuDevice, DisplayMode};
pub use virtio_queue::{VirtQueue, VirtQueueError};

// === Framebuffer导出 ===
pub use framebuffer::{
    Framebuffer, GenericFramebuffer, FramebufferInfo, PixelFormat,
    set_global_framebuffer, get_global_framebuffer, with_global_framebuffer,
};

// === RTC驱动导出 ===
pub use goldfish_rtc::{GoldfishRTCDevice, GoldfishRTCDriver};

// === 驱动子系统初始化 ===

/// 初始化整个驱动子系统
///
/// 这是驱动子系统的主要入口点，将：
/// 1. 初始化HAL层
/// 2. 注册所有驱动程序
/// 3. 扫描和初始化设备
/// 4. 设置中断处理
/// 5. 初始化文件系统
pub fn init_driver_subsystem() {
    info!("[Drivers] Initializing LiteOS driver subsystem");

    // 初始化设备管理器（包含HAL初始化）
    device_manager::init();

    info!("[Drivers] Driver subsystem initialization completed");

    // 显示系统状态
    get_device_statistics();
}

/// 驱动子系统状态检查
pub fn system_health_check() -> bool {
    info!("[Drivers] Performing system health check");

    let mut healthy = true;

    // 检查块设备
    let block_devices = get_all_block_devices();
    if block_devices.is_empty() {
        warn!("[Drivers] No block devices available");
        healthy = false;
    } else {
        info!("[Drivers] {} block device(s) available", block_devices.len());
    }

    // 检查设备管理器状态
    let manager = device_manager();
    let mgr = manager.lock();
    let stats = mgr.get_device_stats();

    let total_devices: usize = stats.values().sum();
    let failed_devices = stats.get(&DeviceState::Failed).unwrap_or(&0);
    let error_devices = stats.get(&DeviceState::Error).unwrap_or(&0);

    info!("[Drivers] Total devices: {}, Failed: {}, Error: {}",
          total_devices, failed_devices, error_devices);

    if *failed_devices > 0 || *error_devices > 0 {
        warn!("[Drivers] Some devices are in failed/error state");
        healthy = false;
    }

    if healthy {
        info!("[Drivers] System health check passed ✓");
    } else {
        warn!("[Drivers] System health check failed ✗");
    }

    healthy
}

/// 驱动子系统关闭
pub fn shutdown_driver_subsystem() -> Result<(), DeviceError> {
    info!("[Drivers] Shutting down driver subsystem");

    // 挂起所有设备
    suspend_all_devices()?;

    info!("[Drivers] Driver subsystem shutdown completed");
    Ok(())
}

/// 获取驱动子系统版本信息
pub fn get_subsystem_info() -> DriverSubsystemInfo {
    DriverSubsystemInfo {
        version: "2.0.0",
        hal_version: "1.0.0",
        supported_buses: vec!["MMIO", "PCI", "Platform", "VirtIO"],
        supported_devices: vec!["Block", "Console", "RTC", "Network", "Display"],
        features: vec![
            "Hot-plug support",
            "Power management",
            "Multi-core interrupt handling",
            "Resource conflict detection",
            "Device statistics",
            "Async I/O support"
        ],
    }
}

/// 驱动子系统信息结构
#[derive(Debug, Clone)]
pub struct DriverSubsystemInfo {
    pub version: &'static str,
    pub hal_version: &'static str,
    pub supported_buses: alloc::vec::Vec<&'static str>,
    pub supported_devices: alloc::vec::Vec<&'static str>,
    pub features: alloc::vec::Vec<&'static str>,
}

// === 兼容性别名（逐步废弃） ===

/// @deprecated 使用 device_manager::init() 替代
#[deprecated(note = "Use device_manager::init() instead")]
pub fn init_deprecated() {
    device_manager::init();
}

/// @deprecated 使用 device_manager::handle_external_interrupt() 替代
#[deprecated(note = "Use device_manager::handle_external_interrupt() instead")]
pub fn handle_external_interrupt_deprecated() {
    device_manager::handle_external_interrupt();
}

/// @deprecated 使用新的HAL Device抽象替代
#[deprecated(note = "Use new HAL Device abstraction instead")]
pub fn hal_devices() -> alloc::vec::Vec<alloc::sync::Arc<dyn Device>> {
    alloc::vec::Vec::new() // 返回空列表，促使迁移到新API
}

/// @deprecated 使用新的HAL设备管理器替代
#[deprecated(note = "Use new HAL device manager instead")]
pub fn with_interrupt_controller<F, R>(_f: F) -> Option<R>
where
    F: FnOnce(&hal::interrupt::BasicInterruptController) -> R,
{
    None // 强制迁移到新的中断处理架构
}