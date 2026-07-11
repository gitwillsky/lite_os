pub(crate) mod block;
mod goldfish_rtc;
mod hal;
mod platform;
mod virtio_blk;
mod virtio_queue;

pub(crate) use goldfish_rtc::GoldfishRTCDevice;
use hal::{
    InterruptController, InterruptError, InterruptHandler, InterruptVector, MmioBus,
    PlicInterruptController, VIRTIO_CONFIG_S_DRIVER_OK, VIRTIO_CONFIG_S_FEATURES_OK,
    VIRTIO_MMIO_INT_CONFIG, VIRTIO_MMIO_INT_VRING, VirtIODevice,
};
use virtio_blk::VirtIOBlockDevice;

/// 初始化整个驱动子系统
///
/// 1. 初始化 PLIC。
/// 2. 扫描 DTB VirtIO MMIO 区间并选定唯一 block device。
pub(crate) fn init() {
    info!("[Drivers] Initializing LiteOS driver subsystem");

    platform::init();

    info!("[Drivers] Driver subsystem initialization completed");
}

/// @description 处理当前 hart 的 PLIC supervisor external interrupt。
pub(crate) fn handle_external_interrupt() {
    platform::handle_external_interrupt();
}
