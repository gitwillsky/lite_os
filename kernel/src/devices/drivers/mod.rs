mod audio_output;
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

pub(crate) use audio_output::{
    PCM_BUFFER_BYTES, PCM_BUFFER_FRAMES, PCM_CHANNELS, PCM_FRAME_BYTES, PCM_PERIOD_BYTES,
    PCM_PERIOD_FRAMES, PCM_PERIODS, PCM_RATE, PcmCompletionObserver, PcmOutput, PcmOutputError,
};
pub(crate) use display::{DisplayDevice, DisplayError, DisplayMode, DisplayRect, DisplayUpdate};
pub(crate) use entropy::EntropySource;
pub(crate) use graphics::{
    CursorCommand, GraphicsDevice, VirglBox, VirglCapsetInfo, VirglCommand, VirglTransferDirection,
};
pub(crate) use hal::{
    BusError, InterruptError, InterruptHandler, InterruptVector, MmioBus, before_mmio_write,
    wait_for_external_interrupt,
};
pub(crate) use input::{InputAbsInfo, InputDevice, InputDeviceError, InputId, RawInputEvent};
pub(crate) use port::{PortActivity, PortDevice, PortError};
pub(crate) use registry::{
    console_device, dispatch_io_completion_work, display_device, fill_entropy, input_device,
    input_device_count, network_device, pcm_output, port_device, register_completion_source,
    register_display_device, register_entropy_source, register_input_device,
    register_network_device, register_pcm_output, register_port_device,
};
