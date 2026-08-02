mod audio_output;
pub(crate) mod block;
mod display;
mod hal;
mod input;
pub(crate) mod io_completion;
pub(crate) mod network;
mod uart;
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
pub(crate) use display::{
    DisplayDevice, DisplayError, DisplayMode, DisplayRect, DisplayUpdate, primary_display,
};
#[cfg(target_arch = "aarch64")]
pub(crate) use hal::PciTransport;
pub(crate) use hal::{InterruptError, InterruptHandler, InterruptVector, MmioBus};
use hal::{
    VIRTIO_CONFIG_S_DRIVER_OK, VIRTIO_CONFIG_S_FEATURES_OK, VIRTIO_F_VERSION_1,
    VIRTIO_MMIO_INT_CONFIG, VIRTIO_MMIO_INT_VRING, VirtIODevice,
};
pub(crate) use input::{InputAbsInfo, InputDevice, InputDeviceError, InputId, RawInputEvent};
pub(crate) use input::{device as input_device, device_count as input_device_count};
pub(crate) use virtio_blk::VirtIOBlockDevice;
pub(crate) use virtio_console::{PortError, VirtIOConsoleDevice};
pub(crate) use virtio_gpu::VirtIOGpuDevice;
pub(crate) use virtio_input::VirtIOInputDevice;
pub(crate) use virtio_net::VirtIONetworkDevice;
pub(crate) use virtio_rng::VirtIORngDevice;
pub(crate) use virtio_sound::VirtIOSoundDevice;

use spin::Once;

// OWNER: drivers registry retains the only physical PCM output adapter after platform discovery.
// 缺少单一 publication 会让两个 ALSA owner 分别控制同一 VirtIO stream。
static AUDIO_OUTPUT: Once<alloc::sync::Arc<VirtIOSoundDevice>> = Once::new();
// OWNER: drivers registry retains the only physical named VirtIO port after platform discovery.
// Missing single publication would let two byte-stream owners race one SPICE vdagent channel.
static VIRTIO_PORT: Once<alloc::sync::Arc<VirtIOConsoleDevice>> = Once::new();

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

/// @description 注册唯一 physical PCM output adapter。
/// @param device platform 已完成 DTB identity 与 IRQ 装配的 VirtIO Sound adapter。
/// @return 首次发布成功；重复设备返回原子失败。
pub(crate) fn register_audio_output(device: alloc::sync::Arc<VirtIOSoundDevice>) -> Result<(), ()> {
    if AUDIO_OUTPUT.get().is_some() {
        return Err(());
    }
    AUDIO_OUTPUT.call_once(|| device);
    Ok(())
}

/// @description Register the unique physical VirtIO Console clipboard port.
/// @param device Platform-proven modern VirtIO Console adapter.
/// @return The first publication succeeds; a duplicate device fails atomically.
pub(crate) fn register_virtio_port(
    device: alloc::sync::Arc<VirtIOConsoleDevice>,
) -> Result<(), ()> {
    if VIRTIO_PORT.get().is_some() {
        return Err(());
    }
    VIRTIO_PORT.call_once(|| device);
    Ok(())
}

/// @description Return the transport-neutral byte stream used by the vdagent service.
/// @return The selected port, or `None` when the platform did not publish one.
pub(crate) fn primary_virtio_port() -> Option<alloc::sync::Arc<VirtIOConsoleDevice>> {
    VIRTIO_PORT.get().cloned()
}

/// @description 取得 kernel audio owner 可消费的 transport-neutral PCM seam。
/// @return platform 未发现 sound device 时为 `None`。
pub(crate) fn primary_audio_output() -> Option<alloc::sync::Arc<dyn PcmOutput>> {
    AUDIO_OUTPUT
        .get()
        .map(|device| device.clone() as alloc::sync::Arc<dyn PcmOutput>)
}

/// @description 在 task/idle safe point 各回收一批有界 driver I/O completion。
///
/// @return 任一设备仍有 backlog 时返回 `true`，caller 必须重新发布 `DriverIo` work。
pub(crate) fn dispatch_io_completion_work() -> bool {
    block::dispatch_completion_work()
        | virtio_rng::dispatch_completion_work()
        | AUDIO_OUTPUT
            .get()
            .is_some_and(|device| virtio_sound::dispatch_completion_work(device))
}

pub(crate) fn register_display_device(
    device: alloc::sync::Arc<dyn DisplayDevice>,
) -> Result<(), ()> {
    display::register(device)
}

pub(crate) fn initialize_console_input() -> Result<(), InterruptError> {
    uart::init()
}

/// @description 由 platform UART hardirq 发布已 drain 的 bounded RX batch。
pub(crate) fn publish_console_input(bytes: &[u8]) {
    uart::publish_received(bytes);
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

/// @description 原子丢弃唯一 UART RX ring 中尚未消费的输入。
/// @return 被丢弃的 byte 数。
pub(crate) fn discard_console_input() -> usize {
    uart::discard_input()
}
