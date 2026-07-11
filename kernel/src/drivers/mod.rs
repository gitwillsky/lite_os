pub(crate) mod block;
pub(crate) mod goldfish_rtc;
pub(crate) mod hal;
pub(crate) mod platform;
pub(crate) mod virtio_blk;
pub(crate) mod virtio_queue;

/// 初始化整个驱动子系统
///
/// 1. 初始化 PLIC。
/// 2. 扫描 DTB VirtIO MMIO 区间并选定唯一 block device。
pub(crate) fn init() {
    info!("[Drivers] Initializing LiteOS driver subsystem");

    platform::init();

    info!("[Drivers] Driver subsystem initialization completed");
}
