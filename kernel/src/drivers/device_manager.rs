use alloc::sync::Arc;
use alloc::vec::Vec;
use alloc::boxed::Box;

use crate::board::board_info;
use crate::drivers::hal::{
    DeviceManager, Device, DeviceType, DeviceError,
    interrupt::{PlicInterruptController, InterruptPriority},
    resource::SystemResourceManager,
};
use crate::drivers::GenericBlockDriver;
use crate::drivers::{VirtIOBlockDevice, BlockDevice, register_block_device};
use crate::drivers::goldfish_rtc::GoldfishRTCDevice;
use crate::drivers::VirtioGpuDevice;
use crate::fs::{FAT32FileSystem, Ext2FileSystem};
use crate::fs::vfs::vfs;

/// 全局HAL设备管理器
static DEVICE_MANAGER: spin::Once<spin::Mutex<DeviceManager>> = spin::Once::new();

/// 获取全局设备管理器
pub fn device_manager() -> &'static spin::Mutex<DeviceManager> {
    DEVICE_MANAGER.call_once(|| {
        let resource_manager = Box::new(SystemResourceManager::new());
        let mut manager = DeviceManager::new(resource_manager);

        // 初始化PLIC中断控制器 - 使用从DTB解析的地址
        let board_info = crate::board::board_info();
        if let Some(plic_dev) = &board_info.plic_device {
            if let Ok(plic) = PlicInterruptController::new(plic_dev.base_addr, 1024, 8) {
                let interrupt_controller = Arc::new(spin::Mutex::new(plic));
                manager = manager.with_interrupt_controller(interrupt_controller);
            }
        }

        spin::Mutex::new(manager)
    })
}

/// 系统初始化入口点
pub fn init() {
    // 注册驱动程序
    register_drivers();
    // 扫描和初始化设备
    scan_and_init_devices();
    // 初始化文件系统
    init_filesystems();

    info!("[DeviceManager] Device initialization completed");
}

/// 注册所有驱动程序
fn register_drivers() {
    let manager = device_manager();
    let mut mgr = manager.lock();

    // 注册通用块设备驱动
    let block_driver = Arc::new(GenericBlockDriver::new());
    if let Err(e) = mgr.register_driver(block_driver) {
        error!("[DeviceManager] Failed to register block driver: {:?}", e);
    }
}

/// 扫描并初始化所有设备
fn scan_and_init_devices() {
    let board_info = board_info();

    // 初始化VirtIO设备
    init_virtio_devices(&board_info);

    // 初始化RTC设备
    init_rtc_devices(&board_info);

    // 枚举所有已注册设备
    enumerate_devices();
}

/// 初始化VirtIO设备
fn init_virtio_devices(board_info: &crate::board::BoardInfo) {
    info!("[DeviceManager] Scanning {} VirtIO devices", board_info.virtio_count);

    // 输出详细的板级信息用于调试
    info!("[DeviceManager] Board info debug:\n{}", board_info);

    for i in 0..board_info.virtio_count {
        if let Some(virtio_dev) = &board_info.virtio_devices[i] {
            let base_addr = virtio_dev.base_addr;
            info!("[DeviceManager] Attempting to probe VirtIO device {} at {:#x}, size={:#x}", i, base_addr, virtio_dev.size);

            // 首先检查设备类型，通过读取device ID
            let device_id = unsafe {
                core::ptr::read_volatile((base_addr + 0x08) as *const u32)
            };
            info!("[DeviceManager] VirtIO device {} has device ID: {:#x}", i, device_id);

            match device_id {
                // VirtIO Block device (0x02)
                2 => {
                    info!("[DeviceManager] Creating VirtIOBlockDevice at {:#x}", base_addr);
                    if let Some(virtio_block) = VirtIOBlockDevice::new(base_addr) {
                        // VirtIOBlockDevice 返回的是 Arc<Self>，我们需要将其转换
                        let virtio_arc = virtio_block.clone();

                        // 直接注册到块设备管理器
                        match register_block_device(virtio_arc.clone()) {
                            Ok(device_id) => {
                                info!("[DeviceManager] VirtIO Block device #{} registered at {:#x}",
                                      device_id, base_addr);
                            }
                            Err(e) => {
                                error!("[DeviceManager] Failed to register block device: {:?}", e);
                            }
                        }

                        // 注册中断处理器（如果有PLIC）
                        // 使用全局设备管理器注册中断处理器（如已配置）
                        if let Some(plic_dev) = &board_info.plic_device {
                            let irq = virtio_dev.irq;
                            if irq != 0 {
                                let manager = device_manager();
                                let mut dm = manager.lock();
                                // 通过公共API获取中断控制器（若暴露），否则跳过
                                if let Some(controller) = dm.get_interrupt_controller() {
                                    let handler = virtio_block.irq_handler_for();
                                    let mut ctrl = controller.lock();
                                    if let Err(e) = ctrl.register_handler(irq, handler.clone()) {
                                        error!("[DeviceManager] Failed to register blk IRQ handler: {:?}", e);
                                    } else if let Err(e) = ctrl.set_priority(irq, InterruptPriority::High) {
                                        error!("[DeviceManager] Failed to set blk IRQ priority: {:?}", e);
                                    } else if let Err(e) = ctrl.enable_interrupt(irq) {
                                        error!("[DeviceManager] Failed to enable blk IRQ {}: {:?}", irq, e);
                                    } else {
                                        info!("[DeviceManager] Registered blk IRQ handler on vector {}", irq);
                                    }
                                }
                            }
                        }
                    } else {
                        warn!("[DeviceManager] Failed to create VirtIO Block device at {:#x}", base_addr);
                    }
                }

                // VirtIO GPU device (0x10)
                16 => {
                    info!("[DeviceManager] Creating VirtioGpuDevice at {:#x}", base_addr);
                    match VirtioGpuDevice::new(base_addr, 0) {
                        Ok(mut gpu_device) => {
                            // 探测设备
                            if let Ok(true) = gpu_device.probe() {
                                // 初始化设备
                                if let Ok(()) = gpu_device.initialize() {
                                    // 注册到设备管理器
                                    let device = Box::new(gpu_device);
                                    let manager = device_manager();
                                    let mut mgr = manager.lock();

                                    match mgr.add_device(device) {
                                        Ok(device_id) => {
                                            info!("[DeviceManager] VirtIO GPU device #{} registered at {:#x}",
                                                  device_id, base_addr);
                                            // 注册中断处理器（如果有PLIC）
                                            if let Some(plic_dev) = &board_info.plic_device {
                                                let irq = virtio_dev.irq;
                                                if irq != 0 {
                                                    if let Some(controller) = mgr.get_interrupt_controller() {
                                                        let mut ctrl = controller.lock();
                                                        // 直接使用统一的 GPU IRQ 处理器
                                                        let handler = Arc::new(crate::drivers::virtio_gpu::VirtioGpuIrqHandler);
                                                        if let Err(e) = ctrl.register_handler(irq, handler.clone()) {
                                                            error!("[DeviceManager] Failed to register GPU IRQ handler: {:?}", e);
                                                        } else if let Err(e) = ctrl.set_priority(irq, InterruptPriority::High) {
                                                            error!("[DeviceManager] Failed to set GPU IRQ priority: {:?}", e);
                                                        } else if let Err(e) = ctrl.enable_interrupt(irq) {
                                                            error!("[DeviceManager] Failed to enable GPU IRQ {}: {:?}", irq, e);
                                                        } else {
                                                            info!("[DeviceManager] Registered GPU IRQ handler on vector {}", irq);
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                        Err(e) => {
                                            error!("[DeviceManager] Failed to add GPU device: {:?}", e);
                                        }
                                    }
                                } else {
                                    error!("[DeviceManager] Failed to initialize GPU device at {:#x}", base_addr);
                                }
                            } else {
                                warn!("[DeviceManager] GPU device probe failed at {:#x}", base_addr);
                            }
                        }
                        Err(e) => {
                            error!("[DeviceManager] Failed to create VirtIO GPU device at {:#x}: {:?}", base_addr, e);
                        }
                    }
                }

                _ => {
                    info!("[DeviceManager] Unrecognized VirtIO device ID {:#x} at {:#x}", device_id, base_addr);
                }
            }
        }
    }
}

/// 初始化RTC设备
fn init_rtc_devices(board_info: &crate::board::BoardInfo) {
    if let Some(rtc_info) = board_info.rtc_device.as_ref() {
        info!("[DeviceManager] Initializing RTC device at {:#x}", rtc_info.base_addr);

        match GoldfishRTCDevice::new(rtc_info.clone()) {
            Ok(rtc_device) => {
                let device = Box::new(rtc_device);
                let manager = device_manager();
                let mut mgr = manager.lock();

                match mgr.add_device(device) {
                    Ok(device_id) => {
                        info!("[DeviceManager] RTC device #{} registered at {:#x}",
                              device_id, rtc_info.base_addr);
                    }
                    Err(e) => {
                        error!("[DeviceManager] Failed to add RTC device: {:?}", e);
                    }
                }
            }
            Err(e) => {
                error!("[DeviceManager] Failed to create RTC device: {:?}", e);
            }
        }
    }
}

/// 枚举所有已注册设备
fn enumerate_devices() {
    let manager = device_manager();
    let mgr = manager.lock();

    let devices = mgr.enumerate_devices();
    info!("[DeviceManager] Total devices registered: {}", devices.len());

    for (id, device_type, name, state) in devices {
        info!("[DeviceManager] Device #{}: {} ({:?}) - State: {:?}",
              id, name, device_type, state);
    }

    // 显示设备统计
    let stats = mgr.get_device_stats();
    for (state, count) in stats {
        debug!("[DeviceManager] Devices in {:?} state: {}", state, count);
    }
}

/// 初始化文件系统
fn init_filesystems() {
    use crate::drivers::{get_primary_block_device, get_all_block_devices};

    let block_devices = get_all_block_devices();
    if block_devices.is_empty() {
        warn!("[DeviceManager] No block devices found for filesystem initialization");
        return;
    }

    info!("[DeviceManager] Found {} block device(s) for filesystem", block_devices.len());

    // 使用第一个块设备初始化文件系统
    if let Some(primary_device) = get_primary_block_device() {
        info!("[DeviceManager] Attempting filesystem initialization on primary block device");

        // 尝试Ext2文件系统
        if let Ok(ext2_fs) = Ext2FileSystem::new(primary_device.clone()) {
            match vfs().mount("/", ext2_fs) {
                Ok(()) => {
                    info!("[DeviceManager] Ext2 filesystem mounted successfully at /");
                    return;
                }
                Err(e) => {
                    warn!("[DeviceManager] Ext2 filesystem mount failed: {:?}", e);
                }
            }
        }

        // 回退到FAT32文件系统
        if let Ok(fat32_fs) = FAT32FileSystem::new(primary_device) {
            match vfs().mount("/", fat32_fs) {
                Ok(()) => {
                    info!("[DeviceManager] FAT32 filesystem mounted successfully at /");
                }
                Err(e) => {
                    error!("[DeviceManager] FAT32 filesystem mount failed: {:?}", e);
                }
            }
        } else {
            error!("[DeviceManager] Unable to create any supported filesystem");
        }
    }
}

/// 处理外部中断
pub fn handle_external_interrupt() {
    // 先短暂获取控制器引用，再释放设备管理器锁，避免在中断回调中重入造成死锁
    let controller_opt = {
        let manager = device_manager();
        let mgr = manager.lock();
        mgr.get_interrupt_controller()
    };

    if let Some(controller) = controller_opt {
        let mut ctrl = controller.lock();
        let vectors = ctrl.pending_interrupts();
        for vector in vectors {
            if let Err(e) = ctrl.handle_interrupt(vector) {
                #[cfg(debug_assertions)]
                debug!("[DeviceManager] Interrupt {} handling failed: {:?}", vector, e);
            }
        }
    }
}

/// 挂起所有设备（电源管理）
pub fn suspend_all_devices() -> Result<(), DeviceError> {
    let manager = device_manager();
    let mgr = manager.lock();

    info!("[DeviceManager] Suspending all devices");
    mgr.suspend_all_devices()
}

/// 恢复所有设备（电源管理）
pub fn resume_all_devices() -> Result<(), DeviceError> {
    let manager = device_manager();
    let mgr = manager.lock();

    info!("[DeviceManager] Resuming all devices");
    mgr.resume_all_devices()
}

/// 获取设备统计信息
pub fn get_device_statistics() {
    let manager = device_manager();
    let mgr = manager.lock();

    let devices = mgr.enumerate_devices();
    let stats = mgr.get_device_stats();

    info!("=== Device Manager Statistics ===");
    info!("Total devices: {}", devices.len());

    for (state, count) in stats {
        info!("  {:?}: {} devices", state, count);
    }

    info!("Device Details:");
    for (id, device_type, name, state) in devices {
        info!("  #{}: {} ({:?}) - {:?}", id, name, device_type, state);
    }
    info!("=== End Statistics ===");
}

/// 按类型查找设备
pub fn find_devices_by_type(device_type: DeviceType) -> Vec<u32> {
    let manager = device_manager();
    let mgr = manager.lock();
    mgr.find_devices_by_type(device_type)
}

/// 按驱动名称查找设备
pub fn find_devices_by_driver(driver_name: &str) -> Vec<u32> {
    let manager = device_manager();
    let mgr = manager.lock();
    mgr.find_devices_by_driver(driver_name)
}

/// 获取设备引用
pub fn get_device(device_id: u32) -> Option<Arc<spin::Mutex<Box<dyn Device>>>> {
    let manager = device_manager();
    let mgr = manager.lock();
    mgr.get_device(device_id)
}