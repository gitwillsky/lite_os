use crate::drivers::device_manager::get_device_statistics;

pub mod block; // 块设备抽象层
pub mod device_manager; // 设备管理器 - 统一设备管理
pub mod goldfish_rtc;
pub mod hal; // 硬件抽象层 - 核心架构
pub mod virtio_blk; // VirtIO块设备驱动
pub mod virtio_console; // VirtIO控制台驱动
pub mod virtio_queue; // VirtIO队列实现 // Goldfish RTC驱动

// === 驱动子系统初始化 ===

/// 初始化整个驱动子系统
///
/// 这是驱动子系统的主要入口点，将：
/// 1. 初始化HAL层
/// 2. 注册所有驱动程序
/// 3. 扫描和初始化设备
/// 4. 设置中断处理
/// 5. 初始化文件系统
pub fn init() {
    info!("[Drivers] Initializing LiteOS driver subsystem");

    // 初始化设备管理器（包含HAL初始化）
    device_manager::init();

    info!("[Drivers] Driver subsystem initialization completed");

    // 显示系统状态
    get_device_statistics();
}
