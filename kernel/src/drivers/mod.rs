mod audio_output;
pub(crate) mod block;
pub(crate) mod console;
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
    block_device, console_device, dispatch_io_completion_work, display_device, fill_entropy,
    input_device, input_device_count, network_device, pcm_output, port_device,
    register_block_device, register_display_device, register_entropy_source, register_input_device,
    register_network_device, register_pcm_output, register_port_device,
};
pub(crate) use virtio_blk::VirtIOBlockDevice;
pub(crate) use virtio_console::VirtIOConsoleDevice;
pub(crate) use virtio_gpu::VirtIOGpuDevice;
pub(crate) use virtio_input::VirtIOInputDevice;
pub(crate) use virtio_net::VirtIONetworkDevice;
pub(crate) use virtio_rng::VirtIORngDevice;
pub(crate) use virtio_sound::VirtIOSoundDevice;
