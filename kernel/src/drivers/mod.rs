pub mod block;
pub mod goldfish_rtc;
pub mod hal;
pub mod platform;
pub mod virtio_blk;
pub mod virtio_queue;

/// 初始化整个驱动子系统
///
/// 1. 初始化 PLIC。
/// 2. 扫描 DTB VirtIO MMIO 区间并选定唯一 block device。
/// 3. 挂载同步读写根文件系统。
pub fn init() {
    info!("[Drivers] Initializing LiteOS driver subsystem");

    platform::init();

    info!("[Drivers] Driver subsystem initialization completed");
}
