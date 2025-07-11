use alloc::sync::Arc;
use alloc::vec::Vec;
use spin::Mutex;

use crate::drivers::{BlockDevice, VirtIOBlockDevice};
use crate::fs::{make_filesystem, vfs::get_vfs, FileSystem};

// QEMU VirtIO 设备的基地址
const VIRTIO_MMIO_BASE: usize = 0x10001000;
const VIRTIO_MMIO_SIZE: usize = 0x1000;
const VIRTIO_MMIO_IRQ: usize = 1;

static DEVICES: Mutex<Vec<Arc<dyn BlockDevice>>> = Mutex::new(Vec::new());

pub fn init_devices() {
    println!("[device] 初始化设备...");
    
    // 扫描VirtIO设备
    scan_virtio_devices();
    
    // 初始化文件系统
    init_filesystems();
}

fn scan_virtio_devices() {
    println!("[device] 扫描VirtIO设备...");
    
    // 在QEMU中，VirtIO设备通常从0x10001000开始，每个设备间隔0x1000
    for i in 0..8 {
        let base_addr = VIRTIO_MMIO_BASE + i * VIRTIO_MMIO_SIZE;
        
        if let Some(device) = VirtIOBlockDevice::new(base_addr) {
            println!("[device] 发现VirtIO块设备，地址: {:#x}", base_addr);
            DEVICES.lock().push(device);
        }
    }
}

fn init_filesystems() {
    println!("[device] 初始化文件系统...");
    
    let devices = DEVICES.lock();
    if devices.is_empty() {
        println!("[device] 警告: 没有找到块设备");
        return;
    }
    
    // 使用第一个块设备作为根文件系统
    let device = devices[0].clone();
    println!("[device] 使用块设备作为根文件系统");
    
    if let Some(fs) = make_filesystem(device) {
        println!("[device] 成功创建文件系统");
        
        // 挂载到根目录
        if let Err(e) = get_vfs().mount("/", fs) {
            println!("[device] 文件系统挂载失败: {:?}", e);
        } else {
            println!("[device] 文件系统挂载成功");
        }
    } else {
        println!("[device] 无法创建文件系统");
    }
}

pub fn get_block_devices() -> Vec<Arc<dyn BlockDevice>> {
    DEVICES.lock().clone()
}