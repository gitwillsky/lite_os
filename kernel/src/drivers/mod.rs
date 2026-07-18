pub(crate) mod block;
mod display;
mod goldfish_rtc;
mod hal;
mod input;
pub(crate) mod network;
mod uart;
mod virtio_blk;
mod virtio_gpu;
mod virtio_input;
mod virtio_net;
mod virtio_queue;
mod virtio_rng;

pub(crate) use display::{
    DisplayDevice, DisplayError, DisplayMode, DisplayRect, DisplayUpdate, primary_display,
};
pub(crate) use goldfish_rtc::GoldfishRTCDevice;
pub(crate) use hal::{
    InterruptController, InterruptError, InterruptHandler, InterruptVector, MmioBus,
};
use hal::{
    VIRTIO_CONFIG_S_DRIVER_OK, VIRTIO_CONFIG_S_FEATURES_OK, VIRTIO_F_VERSION_1,
    VIRTIO_MMIO_INT_CONFIG, VIRTIO_MMIO_INT_VRING, VirtIODevice,
};
pub(crate) use input::{InputAbsInfo, InputDevice, InputDeviceError, InputId, RawInputEvent};
pub(crate) use input::{device as input_device, device_count as input_device_count};
pub(crate) use virtio_blk::VirtIOBlockDevice;
pub(crate) use virtio_gpu::VirtIOGpuDevice;
pub(crate) use virtio_input::VirtIOInputDevice;
pub(crate) use virtio_net::VirtIONetworkDevice;
pub(crate) use virtio_rng::VirtIORngDevice;

pub(crate) use virtio_rng::fill_entropy;

/// Platform backend 可用的窄设备注册 seam。
pub(crate) fn register_input_device(
    device: alloc::sync::Arc<dyn InputDevice>,
) -> Result<usize, alloc::sync::Arc<dyn InputDevice>> {
    input::register(device)
}

pub(crate) fn register_network_device(
    device: alloc::sync::Arc<dyn network::NetworkDevice>,
) -> Result<(), ()> {
    network::register_network_device(device).map_err(|_| ())
}

pub(crate) fn register_entropy_device(device: alloc::sync::Arc<VirtIORngDevice>) -> Result<(), ()> {
    virtio_rng::register(device)
}

pub(crate) fn register_display_device(
    device: alloc::sync::Arc<dyn DisplayDevice>,
) -> Result<(), ()> {
    display::register(device)
}

pub(crate) fn initialize_console_uart(
    base: usize,
    size: usize,
) -> Result<alloc::sync::Arc<dyn InterruptHandler>, InterruptError> {
    uart::init(base, size)
}

pub(crate) fn enable_console_uart_receive() {
    uart::enable_receive_interrupt();
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
