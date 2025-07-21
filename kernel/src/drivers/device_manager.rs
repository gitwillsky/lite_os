use alloc::sync::Arc;
use alloc::vec::Vec;
use spin::Mutex;

use crate::board::board_info;
use crate::drivers::{BlockDevice, VirtIOBlockDevice, init_virtio_console};
use crate::fs::{make_filesystem, vfs::vfs};

static DEVICES: Mutex<Vec<Arc<dyn BlockDevice>>> = Mutex::new(Vec::new());

pub fn init_devices() {
    scan_virtio_devices();

    init_filesystems();
}

fn scan_virtio_devices() {
    let board_info = board_info();

    for i in 0..board_info.virtio_count {
        if let Some(virtio_dev) = &board_info.virtio_devices[i] {
            let base_addr = virtio_dev.base_addr;

            // 首先尝试初始化VirtIO Console设备
            if init_virtio_console(base_addr) {
                debug!("[device] VirtIO Console initialized at {:#x}", base_addr);
            } else {
                if let Some(device) = VirtIOBlockDevice::new(base_addr) {
                    DEVICES.lock().push(device);
                }
            }
        }
    }
}

fn init_filesystems() {
    let devices = DEVICES.lock();
    if devices.is_empty() {
        error!("[device]: No block devices found");
        return;
    }

    // Use the first block device as root file system
    let device = devices[0].clone();

    if let Some(fs) = make_filesystem(device) {
        // Mount to root directory
        if let Err(e) = vfs().mount("/", fs) {
            error!("[device] File system mount failed: {:?}", e);
        } else {
            debug!("[device] File system mounted successfully");
        }
    } else {
        error!("[device] Unable to create file system");
    }
}

pub fn block_devices() -> Vec<Arc<dyn BlockDevice>> {
    DEVICES.lock().clone()
}

pub fn handle_external_interrupt() {
    // 简单的VirtIO中断处理 - 遍历所有设备检查中断状态
    debug!("[device] Handling external interrupt");
    // 这里可以添加具体的设备中断处理逻辑
    // 由于当前使用轮询方式，暂时不需要复杂的中断处理
}
