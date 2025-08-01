use alloc::sync::Arc;
use alloc::vec::Vec;
use alloc::format;
use spin::Mutex;

use crate::board::board_info;
use crate::drivers::{BlockDevice, VirtIOBlockDevice};
use crate::drivers::hal::{Device, DeviceType, BasicInterruptController, InterruptController};
use crate::fs::{make_filesystem, vfs::vfs};

static BLOCK_DEVICES: Mutex<Vec<Arc<dyn BlockDevice>>> = Mutex::new(Vec::new());
static HAL_DEVICES: Mutex<Vec<Arc<dyn Device>>> = Mutex::new(Vec::new());
static INTERRUPT_CONTROLLER: Mutex<Option<BasicInterruptController>> = Mutex::new(None);

pub fn init() {
    init_interrupt_controller();
    scan_virtio_devices();
    init_filesystems();
}

fn init_interrupt_controller() {
    let mut controller = INTERRUPT_CONTROLLER.lock();
    *controller = Some(BasicInterruptController::new());
    debug!("[HAL] Interrupt controller initialized");
}

fn scan_virtio_devices() {
    let board_info = board_info();

    debug!("[device] Found {} VirtIO devices", board_info.virtio_count);

    for i in 0..board_info.virtio_count {
        if let Some(virtio_dev) = &board_info.virtio_devices[i] {
            let base_addr = virtio_dev.base_addr;
            debug!("[device] Scanning VirtIO device {} at {:#x}", i, base_addr);

            if let Some(device) = VirtIOBlockDevice::new(base_addr) {
                // 同时存储为BlockDevice和HAL Device
                let block_device = device.clone() as Arc<dyn BlockDevice>;
                let hal_device = device as Arc<dyn Device>;

                BLOCK_DEVICES.lock().push(block_device);
                HAL_DEVICES.lock().push(hal_device);

                debug!("[device] VirtIO Block device initialized at {:#x}", base_addr);
            } else {
                debug!("[device] Skipping non-block VirtIO device at {:#x}", base_addr);
            }
        }
    }
}


fn init_filesystems() {
    let block_devices = block_devices();
    if block_devices.is_empty() {
        error!("[device]: No block devices found");
        return;
    }

    // Use the first block device as root file system
    let device = block_devices[0].clone();

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
    BLOCK_DEVICES.lock().clone()
}

pub fn hal_devices() -> Vec<Arc<dyn Device>> {
    HAL_DEVICES.lock().clone()
}

pub fn with_interrupt_controller<F, R>(f: F) -> Option<R>
where
    F: FnOnce(&BasicInterruptController) -> R,
{
    let controller = INTERRUPT_CONTROLLER.lock();
    controller.as_ref().map(f)
}

pub fn handle_external_interrupt() {
    debug!("[device] Handling external interrupt");

    if let Some(controller) = INTERRUPT_CONTROLLER.lock().as_ref() {
        let pending = controller.pending_interrupts();
        for vector in pending {
            if let Err(e) = controller.handle_interrupt(vector) {
                debug!("[HAL] Failed to handle interrupt {}: {}", vector, e);
            }
        }
    }
}
