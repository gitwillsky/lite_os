//! 设备类注册表：platform 按发现顺序发布同一 seam 的多个 adapter。
//!
//! 每个设备类（network、display、input、PCM output、USB、entropy、named port）各有一个只追加的
//! 注册表；index 是 adapter 的稳定 identity。选择哪个实例是消费领域的策略，注册表不区分“主设备”。
//! 注册表是进程内的：驱动宿主登记驱动 adapter，使用者登记对应的远端 client。

use alloc::sync::Arc;
use core::mem::MaybeUninit;

use crate::{
    AppendOnlyRegistry, EntropyError, EntropySource, GraphicsDevice, InputDevice, PcmOutput,
    PortDevice, UsbDevice,
};

// OWNER: display adapter 的唯一发布点；IRQ handler 与 DRM 持有同一 Arc，缺失时 scanout backing
// 生命周期由两方各自决定。
static DISPLAY_DEVICES: AppendOnlyRegistry<dyn GraphicsDevice> = AppendOnlyRegistry::new();
// OWNER: input adapter 按发现顺序的唯一发布点；index 即 evdev minor，缺失时 devfs event 号与 IRQ
// adapter 身份分裂。
static INPUT_DEVICES: AppendOnlyRegistry<dyn InputDevice> = AppendOnlyRegistry::new();
// OWNER: PCM playback adapter 的唯一发布点；缺失时两个 ALSA owner 可能控制同一 stream。
static PCM_OUTPUTS: AppendOnlyRegistry<dyn PcmOutput> = AppendOnlyRegistry::new();
// OWNER: 每次 USB 连接只发布一个 adapter；永久 index 使旧 sysfs remove 不会命中新连接。
// adapter 查询 host generation，不复制在线状态；缺少此来源则托盘只能猜测声卡名称。
static USB_DEVICES: AppendOnlyRegistry<dyn UsbDevice> = AppendOnlyRegistry::new();

/// 发布真实 USB 连接；扩容失败时原样返回 adapter。
pub fn register_usb_device(device: Arc<dyn UsbDevice>) -> Result<usize, Arc<dyn UsbDevice>> {
    USB_DEVICES.register(device)
}
/// 返回稳定 index 对应的连接；在线状态由 adapter 裁决。
pub fn usb_device(index: usize) -> Option<Arc<dyn UsbDevice>> {
    USB_DEVICES.get(index)
}
/// 已发布的连接数；已断开的连接可能仍由旧 sysfs inode 引用。
pub fn usb_device_count() -> usize {
    USB_DEVICES.count()
}
// OWNER: entropy source 的唯一发布点；`getrandom` 与 `/dev/random` 共用首个 source。
static ENTROPY_SOURCES: AppendOnlyRegistry<dyn EntropySource> = AppendOnlyRegistry::new();
// OWNER: named byte-stream port 的唯一发布点；缺失时两个 byte-stream owner 可能竞争同一 SPICE
// channel。
static PORT_DEVICES: AppendOnlyRegistry<dyn PortDevice> = AppendOnlyRegistry::new();

/// 发布一个 display adapter。
///
/// # Errors
///
/// 注册表扩容失败时原样返回 adapter。
pub fn register_display_device(
    device: Arc<dyn GraphicsDevice>,
) -> Result<usize, Arc<dyn GraphicsDevice>> {
    DISPLAY_DEVICES.register(device)
}

/// 第 `index` 个 display adapter。
pub fn display_device(index: usize) -> Option<Arc<dyn GraphicsDevice>> {
    DISPLAY_DEVICES.get(index)
}

/// 按发现顺序发布一个 input adapter；index 即 `/dev/input/eventN` 的 N。
///
/// # Errors
///
/// 注册表扩容失败时原样返回 adapter。
pub fn register_input_device(device: Arc<dyn InputDevice>) -> Result<usize, Arc<dyn InputDevice>> {
    INPUT_DEVICES.register(device)
}

/// 第 `index` 个 input adapter。
pub fn input_device(index: usize) -> Option<Arc<dyn InputDevice>> {
    INPUT_DEVICES.get(index)
}

/// 已发布 input adapter 数量。
pub fn input_device_count() -> usize {
    INPUT_DEVICES.count()
}

/// 发布一个 PCM playback adapter。
///
/// # Errors
///
/// 注册表扩容失败时原样返回 adapter。
pub fn register_pcm_output(device: Arc<dyn PcmOutput>) -> Result<usize, Arc<dyn PcmOutput>> {
    PCM_OUTPUTS.register(device)
}

/// 第 `index` 个 PCM playback adapter。
pub fn pcm_output(index: usize) -> Option<Arc<dyn PcmOutput>> {
    PCM_OUTPUTS.get(index)
}

/// 发布一个 entropy source。
///
/// # Errors
///
/// 注册表扩容失败时原样返回 adapter。
pub fn register_entropy_source(
    device: Arc<dyn EntropySource>,
) -> Result<usize, Arc<dyn EntropySource>> {
    ENTROPY_SOURCES.register(device)
}

/// 用首个 entropy source 完整初始化 caller-owned output。
///
/// # Errors
///
/// 没有 entropy source 或设备失败返回 [`EntropyError`]。
pub fn fill_entropy(bytes: &mut [MaybeUninit<u8>]) -> Result<(), EntropyError> {
    ENTROPY_SOURCES.get(0).ok_or(EntropyError)?.fill(bytes)
}

/// 发布一个 named byte-stream port。
///
/// # Errors
///
/// 注册表扩容失败时原样返回 adapter。
pub fn register_port_device(device: Arc<dyn PortDevice>) -> Result<usize, Arc<dyn PortDevice>> {
    PORT_DEVICES.register(device)
}

/// 第 `index` 个 named byte-stream port。
pub fn port_device(index: usize) -> Option<Arc<dyn PortDevice>> {
    PORT_DEVICES.get(index)
}
