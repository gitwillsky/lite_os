//! 设备类注册表：platform 按发现顺序发布同一 seam 的多个 adapter。
//!
//! 每个设备类（display、USB、entropy、named port）各有一个只追加的
//! 注册表；index 是 adapter 的稳定 identity。选择哪个实例是消费领域的策略，注册表不区分“主设备”。
//! 注册表是进程内的：驱动宿主登记驱动 adapter，使用者登记对应的远端 client。

use alloc::sync::Arc;
use core::mem::MaybeUninit;

use crate::{AppendOnlyRegistry, EntropyError, EntropySource, GraphicsDevice, UsbDevice};

// OWNER: display adapter 的唯一发布点；IRQ handler 与 DRM 持有同一 Arc，缺失时 scanout backing
// 生命周期由两方各自决定。
static DISPLAY_DEVICES: AppendOnlyRegistry<dyn GraphicsDevice> = AppendOnlyRegistry::new();
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
// OWNER: entropy source 的唯一发布点；`getrandom` 与 `/dev/random` 共用最新的可用 source
// （驱动宿主重启后旧 source 的会话已关闭，新 source 以新 index 登记）。
static ENTROPY_SOURCES: AppendOnlyRegistry<dyn EntropySource> = AppendOnlyRegistry::new();

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

/// 用最新登记且可用的 entropy source 完整初始化 caller-owned output。
///
/// # Errors
///
/// 没有 entropy source 或全部设备失败返回 [`EntropyError`]。
pub fn fill_entropy(bytes: &mut [MaybeUninit<u8>]) -> Result<(), EntropyError> {
    (0..ENTROPY_SOURCES.count())
        .rev()
        .filter_map(|index| ENTROPY_SOURCES.get(index))
        .find_map(|source| source.fill(bytes).ok())
        .ok_or(EntropyError)
}
