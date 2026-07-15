pub(crate) mod block;
mod display;
mod goldfish_rtc;
mod hal;
mod input;
pub(crate) mod network;
mod platform;
mod uart;
mod virtio_blk;
mod virtio_gpu;
mod virtio_input;
mod virtio_net;
mod virtio_queue;
mod virtio_rng;

pub(crate) use display::{
    DisplayDevice, DisplayError, DisplayMode, DisplayUpdate, primary_display,
};
pub(crate) use goldfish_rtc::GoldfishRTCDevice;
use hal::{
    InterruptController, InterruptError, InterruptHandler, InterruptVector, MmioBus,
    PlicInterruptController, VIRTIO_CONFIG_S_DRIVER_OK, VIRTIO_CONFIG_S_FEATURES_OK,
    VIRTIO_F_VERSION_1, VIRTIO_MMIO_INT_CONFIG, VIRTIO_MMIO_INT_VRING, VirtIODevice,
};
pub(crate) use input::{InputAbsInfo, InputDevice, InputDeviceError, InputId, RawInputEvent};
pub(crate) use input::{device as input_device, device_count as input_device_count};
use virtio_blk::VirtIOBlockDevice;
use virtio_gpu::VirtIOGpuDevice;
use virtio_input::VirtIOInputDevice;
use virtio_net::VirtIONetworkDevice;
use virtio_rng::VirtIORngDevice;

pub(crate) use virtio_rng::fill_entropy;

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

/// @description 从唯一 UART RX ring 非阻塞读取 console bytes。
///
/// @param bytes kernel-owned 输出缓冲区。
/// @return 当前已有的输入长度。
pub(crate) fn read_console(bytes: &mut [u8]) -> usize {
    uart::read(bytes)
}

/// @description 查询唯一 UART RX ring 是否可读。
///
/// @return ring 非空时返回 true。
pub(crate) fn console_input_ready() -> bool {
    uart::input_ready()
}
