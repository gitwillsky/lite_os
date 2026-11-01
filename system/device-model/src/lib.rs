#![no_std]

//! 设备模型：驱动宿主与设备使用者之间的设备类契约。
//!
//! 1. 每个设备类是一个 trait（块设备、网卡、显示/图形、输入、PCM 输出、named port、entropy、USB）；
//!    驱动宿主中由驱动实现，使用者进程中由远端 client 实现同一 trait，上层代码不区分两者。
//! 2. 注册表与 [`DeviceBacking`] 在各进程内独立存在：驱动宿主登记驱动 adapter，使用者登记远端 client。
//! 3. 设备主动产生的活动经 [`Activity`] 通知使用者；使用者随后按 trait 拉取状态。

extern crate alloc;

mod append_registry;
mod audio_output;
mod backing;
mod block;
mod display;
mod entropy;
mod graphics;
mod input;
pub mod network;
mod pcm_format;
mod port;
mod registry;
mod usb;

pub use append_registry::AppendOnlyRegistry;
pub use audio_output::{
    PCM_BUFFER_FRAMES, PCM_CHANNELS, PCM_PERIOD_BYTES, PCM_PERIOD_FRAMES, PCM_PERIODS,
    PcmCompletionObserver, PcmOutput, PcmOutputError,
};
pub use backing::{DeviceBacking, DeviceExtent};
pub use block::{BLOCK_SIZE, BlockDevice, BlockError};
pub use display::{
    BootFramebuffer, DisplayDevice, DisplayError, DisplayMode, DisplayRect, DisplayUpdate,
};
pub use entropy::{EntropyError, EntropySource};
pub use graphics::{
    CursorCommand, GraphicsDevice, VIRGL_COMMAND_EXTENTS_MAX, VirglBox, VirglCapsetInfo,
    VirglCommand, VirglTransferDirection,
};
pub use input::{InputAbsInfo, InputDevice, InputDeviceError, InputId, RawInputEvent};
pub use pcm_format::{
    MAX_PCM_PERIOD_BYTES, PCM_RATES, PcmCapabilities, PcmConfiguration, PcmFormat,
};
pub use port::{PortActivity, PortDevice, PortError};
pub use registry::{
    display_device, fill_entropy, input_device, input_device_count, pcm_output, port_device,
    register_display_device, register_entropy_source, register_input_device, register_pcm_output,
    register_port_device, register_usb_device, usb_device, usb_device_count,
};
pub use usb::{UsbDevice, UsbIdentity, UsbRemoveError};

/// 设备活动通知：驱动在完成、输入或状态变化时调用，使用者据此安排一次拉取。
///
/// 实现不得睡眠（驱动在中断与完成路径上调用它）；多次通知可以合并。
pub type Activity = alloc::sync::Arc<dyn Fn() + Send + Sync>;
