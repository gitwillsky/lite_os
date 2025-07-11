use alloc::sync::Arc;
use alloc::vec::Vec;
use spin::Mutex;

use crate::drivers::{BlockDevice, VirtIOBlockDevice};
use crate::fs::{make_filesystem, vfs::get_vfs, FileSystem};

// VirtIO device base address in QEMU
const VIRTIO_MMIO_BASE: usize = 0x10001000;
const VIRTIO_MMIO_SIZE: usize = 0x1000;
const VIRTIO_MMIO_IRQ: usize = 1;

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
    
    // In QEMU, VirtIO devices typically start at 0x10001000, with 0x1000 spacing
    for i in 0..8 {
        let base_addr = VIRTIO_MMIO_BASE + i * VIRTIO_MMIO_SIZE;
        
        if let Some(device) = VirtIOBlockDevice::new(base_addr) {
            println!("[device] Found VirtIO block device at address: {:#x}", base_addr);
            DEVICES.lock().push(device);
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