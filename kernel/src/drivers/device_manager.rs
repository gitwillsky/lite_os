use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::arch::dtb::{BoardInfo, board_info};
use crate::drivers::block::{
    GenericBlockDriver, get_all_block_devices, get_primary_block_device, register_block_device,
};
use crate::drivers::goldfish_rtc::GoldfishRTCDevice;
use crate::drivers::hal::device::{Device, DeviceManager, DeviceType};
use crate::drivers::hal::interrupt::{
    InterruptController, InterruptHandler, PlicInterruptController,
};
use crate::drivers::hal::resource::SystemResourceManager;
use crate::drivers::virtio_blk::VirtIOBlockDevice;
use crate::fs::Ext2FileSystem;
use crate::fs::vfs::vfs;
use crate::sync::IrqMutex;

/// 全局HAL设备管理器
static DEVICE_MANAGER: spin::Once<IrqMutex<DeviceManager>> = spin::Once::new();

/// 获取全局设备管理器
pub fn device_manager() -> &'static IrqMutex<DeviceManager> {
    DEVICE_MANAGER.call_once(|| {
        let resource_manager = Box::new(SystemResourceManager::new());
        let mut manager = DeviceManager::new(resource_manager);

        // 初始化PLIC中断控制器 - 使用从DTB解析的地址
        let board_info = board_info();
        if let Some(plic_dev) = &board_info.plic_device {
            if let Ok(plic) = PlicInterruptController::new(plic_dev.base_addr, 1024, 8) {
                let interrupt_controller =
                    Arc::new(IrqMutex::new(Box::new(plic) as Box<dyn InterruptController>));
                manager = manager.with_interrupt_controller(interrupt_controller);
            }
        }

        IrqMutex::new(manager)
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
    let mgr = manager.lock();

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
fn init_virtio_devices(board_info: &BoardInfo) {
    info!(
        "[DeviceManager] Scanning {} VirtIO devices",
        board_info.virtio_count
    );
    info!("[DeviceManager] Board info debug:\n{}", board_info);

    for i in 0..board_info.virtio_count {
        if let Some(virtio_dev) = &board_info.virtio_devices[i] {
            let base_addr = virtio_dev.base_addr;
            info!(
                "[DeviceManager] Attempting to probe VirtIO device {} at {:#x}, size={:#x}",
                i, base_addr, virtio_dev.size
            );
            info!(
                "[DeviceManager] Processing VirtIO device {}/{}",
                i + 1,
                board_info.virtio_count
            );

            let device_id = read_virtio_device_id(base_addr);
            info!(
                "[DeviceManager] VirtIO device {} has device ID: {:#x}",
                i, device_id
            );

            match device_id {
                2 => init_virtio_blk_device(board_info, virtio_dev.irq, base_addr),
                _ => info!(
                    "[DeviceManager] Unrecognized VirtIO device ID {:#x} at {:#x}",
                    device_id, base_addr
                ),
            }
        }
    }
}

#[inline]
fn read_virtio_device_id(base_addr: usize) -> u32 {
    unsafe { core::ptr::read_volatile((base_addr + 0x08) as *const u32) }
}

fn maybe_register_irq(
    board_info: &BoardInfo,
    irq: u32,
    handler: alloc::sync::Arc<dyn InterruptHandler>,
    label: &str,
) {
    if board_info.plic_device.is_none() || irq == 0 {
        return;
    }

    let controller_opt = {
        let manager = device_manager();
        let dm = manager.lock();
        dm.get_interrupt_controller()
    };
    if let Some(controller) = controller_opt {
        let mut ctrl = controller.lock();
        let res = if let Err(e) = ctrl.register_handler(irq, handler.clone()) {
            error!(
                "[DeviceManager] Failed to register {} IRQ handler: {:?}",
                label, e
            );
            Err(())
        } else if let Err(e) = ctrl.set_priority(irq) {
            error!(
                "[DeviceManager] Failed to set {} IRQ priority: {:?}",
                label, e
            );
            Err(())
        } else if ctrl.supports_cpu_affinity() {
            if let Err(e) = ctrl.set_affinity(irq, 1 << 0) {
                warn!(
                    "[DeviceManager] Failed to set {} IRQ affinity: {:?}",
                    label, e
                );
            } else {
                info!("[DeviceManager] Set {} IRQ affinity to CPU0", label);
            }
            if let Err(e) = ctrl.enable_interrupt(irq) {
                error!(
                    "[DeviceManager] Failed to enable {} IRQ {}: {:?}",
                    label, irq, e
                );
                Err(())
            } else {
                info!(
                    "[DeviceManager] Registered {} IRQ handler on vector {}",
                    label, irq
                );
                Ok(())
            }
        } else if let Err(e) = ctrl.enable_interrupt(irq) {
            error!(
                "[DeviceManager] Failed to enable {} IRQ {}: {:?}",
                label, irq, e
            );
            Err(())
        } else {
            info!(
                "[DeviceManager] Registered {} IRQ handler on vector {}",
                label, irq
            );
            Ok(())
        };
        drop(ctrl);
        let _ = res;
    }
}

fn init_virtio_blk_device(board_info: &BoardInfo, irq: u32, base_addr: usize) {
    info!(
        "[DeviceManager] Creating VirtIOBlockDevice at {:#x}",
        base_addr
    );
    if let Some(virtio_block) = VirtIOBlockDevice::new(base_addr) {
        let virtio_arc = virtio_block.clone();
        match register_block_device(virtio_arc.clone()) {
            Ok(device_id) => {
                info!(
                    "[DeviceManager] VirtIO Block device #{} registered at {:#x}",
                    device_id, base_addr
                );
            }
            Err(e) => {
                error!("[DeviceManager] Failed to register block device: {:?}", e);
            }
        }
        maybe_register_irq(board_info, irq, virtio_block.irq_handler_for(), "blk");
    } else {
        warn!(
            "[DeviceManager] Failed to create VirtIO Block device at {:#x}",
            base_addr
        );
    }
}

/// 初始化RTC设备
fn init_rtc_devices(board_info: &BoardInfo) {
    if let Some(rtc_info) = board_info.rtc_device.as_ref() {
        info!(
            "[DeviceManager] Initializing RTC device at {:#x}",
            rtc_info.base_addr
        );

        match GoldfishRTCDevice::new(rtc_info.clone()) {
            Ok(rtc_device) => {
                let device = Box::new(rtc_device);
                let manager = device_manager();
                let mgr = manager.lock();

                match mgr.add_device(device) {
                    Ok(device_id) => {
                        info!(
                            "[DeviceManager] RTC device #{} registered at {:#x}",
                            device_id, rtc_info.base_addr
                        );
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
    info!(
        "[DeviceManager] Total devices registered: {}",
        devices.len()
    );

    for (id, device_type, name, state) in devices {
        info!(
            "[DeviceManager] Device #{}: {} ({:?}) - State: {:?}",
            id, name, device_type, state
        );
    }

    // 显示设备统计
    let stats = mgr.get_device_stats();
    for (state, count) in stats {
        debug!("[DeviceManager] Devices in {:?} state: {}", state, count);
    }
}

/// 初始化文件系统
fn init_filesystems() {
    let block_devices = get_all_block_devices();
    if block_devices.is_empty() {
        warn!("[DeviceManager] No block devices found for filesystem initialization");
        return;
    }

    info!(
        "[DeviceManager] Found {} block device(s) for filesystem",
        block_devices.len()
    );

    // 使用第一个块设备初始化文件系统
    if let Some(primary_device) = get_primary_block_device() {
        info!("[DeviceManager] Attempting filesystem initialization on primary block device");

        match Ext2FileSystem::new(primary_device) {
            Ok(ext2_fs) => match vfs().mount_root(ext2_fs) {
                Ok(()) => info!("[DeviceManager] Ext2 filesystem mounted successfully at /"),
                Err(e) => error!("[DeviceManager] Ext2 root mount failed: {:?}", e),
            },
            Err(e) => {
                error!(
                    "[DeviceManager] Ext2 filesystem initialization failed: {:?}",
                    e
                );
            }
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
        let result = controller.lock().handle_pending_interrupts();
        if let Err(e) = result {
            #[cfg(debug_assertions)]
            debug!("[DeviceManager] Interrupt handling failed: {:?}", e);
        }
    }
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

/// 获取设备引用
pub fn get_device(device_id: u32) -> Option<Arc<spin::Mutex<Box<dyn Device>>>> {
    let manager = device_manager();
    let mgr = manager.lock();
    mgr.get_device(device_id)
}
