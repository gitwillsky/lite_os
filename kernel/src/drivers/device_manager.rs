use alloc::sync::Arc;
use alloc::vec::Vec;
use spin::Mutex;

use crate::board::get_board_info;
use crate::drivers::{BlockDevice, VirtIOBlockDevice};
use crate::fs::{make_filesystem, vfs::get_vfs, FileSystem};

static DEVICES: Mutex<Vec<Arc<dyn BlockDevice>>> = Mutex::new(Vec::new());

pub fn init_devices() {
    println!("[device] Initializing devices...");
    
    // Scan VirtIO devices
    scan_virtio_devices();
    
    // Initialize file systems
    init_filesystems();
}

fn scan_virtio_devices() {
    println!("[device] Scanning VirtIO devices...");
    
    // 从 BoardInfo 获取 VirtIO 设备信息
    let board_info = get_board_info();
    
    println!("[device] Found {} VirtIO devices from device tree", board_info.virtio_count);
    
    for i in 0..board_info.virtio_count {
        if let Some(virtio_dev) = &board_info.virtio_devices[i] {
            let base_addr = virtio_dev.base_addr;
            
            println!("[device] Checking VirtIO device {} at address: {:#x}, IRQ: {}", 
                     i, base_addr, virtio_dev.irq);
            
            if let Some(device) = VirtIOBlockDevice::new(base_addr) {
                println!("[device] Found VirtIO block device at address: {:#x}", base_addr);
                DEVICES.lock().push(device);
            }
        }
    }
}

fn init_filesystems() {
    println!("[device] Initializing file systems...");
    
    let devices = DEVICES.lock();
    if devices.is_empty() {
        println!("[device] Warning: No block devices found");
        return;
    }
    
    // Use the first block device as root file system
    let device = devices[0].clone();
    println!("[device] Using block device as root file system");
    
    if let Some(fs) = make_filesystem(device) {
        println!("[device] Successfully created file system");
        
        // Mount to root directory
        if let Err(e) = get_vfs().mount("/", fs) {
            println!("[device] File system mount failed: {:?}", e);
        } else {
            println!("[device] File system mounted successfully");
        }
    } else {
        println!("[device] Unable to create file system");
    }
}

pub fn get_block_devices() -> Vec<Arc<dyn BlockDevice>> {
    DEVICES.lock().clone()
}

pub fn handle_external_interrupt() {
    // 简单的VirtIO中断处理 - 遍历所有设备检查中断状态
    println!("[device] Handling external interrupt");
    // 这里可以添加具体的设备中断处理逻辑
    // 由于当前使用轮询方式，暂时不需要复杂的中断处理
}