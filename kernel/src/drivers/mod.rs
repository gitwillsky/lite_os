mod audio_output;
pub(crate) mod block;
mod console_input;
mod display;
mod entropy;
mod graphics;
mod hal;
mod input;
pub(crate) mod io_completion;
pub(crate) mod network;
mod port;
mod registry;
mod virtio_blk;
mod virtio_completion_irq;
mod virtio_console;
mod virtio_gpu;
mod virtio_input;
mod virtio_net;
mod virtio_queue;
mod virtio_rng;
mod virtio_sound;

pub(crate) use audio_output::{
    PCM_BUFFER_FRAMES, PCM_FRAME_BYTES, PCM_PERIOD_BYTES, PCM_PERIOD_FRAMES, PCM_RATE,
    PcmCompletionObserver, PcmOutput, PcmOutputError,
};
pub(crate) use display::{DisplayDevice, DisplayError, DisplayMode, DisplayRect, DisplayUpdate};
pub(crate) use entropy::EntropySource;
pub(crate) use graphics::{
    CursorCommand, GraphicsDevice, VirglBox, VirglCapsetInfo, VirglCommand, VirglTransferDirection,
};
pub(crate) use hal::PciTransport;
pub(crate) use hal::{InterruptError, InterruptHandler, InterruptVector, MmioBus};
use hal::{
    VIRTIO_CONFIG_S_DRIVER_OK, VIRTIO_CONFIG_S_FEATURES_OK, VIRTIO_F_VERSION_1,
    VIRTIO_MMIO_INT_CONFIG, VIRTIO_MMIO_INT_VRING, VirtIODevice,
};
pub(crate) use input::{InputAbsInfo, InputDevice, InputDeviceError, InputId, RawInputEvent};
pub(crate) use port::{PortActivity, PortDevice, PortError};
pub(crate) use registry::{
    block_device, dispatch_io_completion_work, display_device, fill_entropy, input_device,
    input_device_count, network_device, pcm_output, port_device, register_block_device,
    register_display_device, register_entropy_source, register_input_device,
    register_network_device, register_pcm_output, register_port_device,
};
pub(crate) use virtio_blk::VirtIOBlockDevice;
pub(crate) use virtio_console::VirtIOConsoleDevice;
pub(crate) use virtio_gpu::VirtIOGpuDevice;
pub(crate) use virtio_input::VirtIOInputDevice;
pub(crate) use virtio_net::VirtIONetworkDevice;
pub(crate) use virtio_rng::VirtIORngDevice;
pub(crate) use virtio_sound::VirtIOSoundDevice;

pub(crate) fn initialize_console_input() -> Result<(), InterruptError> {
    console_input::init()
}

/// 由 platform UART hardirq 发布已 drain 的 bounded RX batch。
pub(crate) fn publish_console_input(bytes: &[u8]) {
    console_input::publish_received(bytes);
}

/// 从唯一 console RX ring 非阻塞读取 console bytes。
///
/// # Parameters
///
/// - `bytes`: kernel-owned 输出缓冲区。
///
/// # Returns
///
/// 当前已有的输入长度。
pub(crate) fn read_console(bytes: &mut [u8]) -> usize {
    console_input::read(bytes)
}

/// 查询唯一 console RX ring 是否可读。
///
/// # Returns
///
/// ring 非空时返回 true。
pub(crate) fn console_input_ready() -> bool {
    console_input::input_ready()
}

/// 原子丢弃唯一 console RX ring 中尚未消费的输入。
///
/// # Returns
///
/// 被丢弃的 byte 数。
pub(crate) fn discard_console_input() -> usize {
    console_input::discard_input()
}
