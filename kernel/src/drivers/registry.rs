//! 设备类注册表：platform 按发现顺序发布同一 seam 的多个 adapter。
//!
//! 每个设备类（block、console、network、display、input、PCM output、entropy、named port）与共享
//! driver-I/O completion 源各有一个只追加的注册表；index 是 adapter 的稳定 identity。选择哪个
//! 实例是消费领域的策略，注册表不区分“主设备”。

use alloc::{sync::Arc, vec::Vec};
use core::mem::MaybeUninit;
use spin::Mutex;

use super::{
    EntropySource, GraphicsDevice, InputDevice, PcmOutput, PortDevice, block::BlockDevice,
    console::ConsoleDevice, io_completion::CompletionSource, network::NetworkDevice,
};

/// 一个设备类的全部已发布 adapter，按注册顺序编号，只追加。
struct DeviceRegistry<T: ?Sized> {
    devices: Mutex<Vec<Arc<T>>>,
}

impl<T: ?Sized> DeviceRegistry<T> {
    const fn new() -> Self {
        Self {
            devices: Mutex::new(Vec::new()),
        }
    }

    fn register(&self, device: Arc<T>) -> Result<usize, Arc<T>> {
        let mut devices = self.devices.lock();
        if devices.try_reserve(1).is_err() {
            return Err(device);
        }
        devices.push(device);
        Ok(devices.len() - 1)
    }

    fn get(&self, index: usize) -> Option<Arc<T>> {
        self.devices.lock().get(index).cloned()
    }

    fn count(&self) -> usize {
        self.devices.lock().len()
    }
}

// OWNER: 块设备 adapter 的唯一发布点；根文件系统按 index 选择。缺失时第二块盘只能 panic 或被丢弃。
static BLOCK_DEVICES: DeviceRegistry<dyn BlockDevice> = DeviceRegistry::new();
// OWNER: Ethernet adapter 的唯一发布点；协议栈与 AF_PACKET 按 index 绑定同一接口，缺失时两者
// 可能持有不同 adapter 而分裂 MAC 与 RX ownership。
static NETWORK_DEVICES: DeviceRegistry<dyn NetworkDevice> = DeviceRegistry::new();
// OWNER: display adapter 的唯一发布点；IRQ handler 与 DRM 持有同一 Arc，缺失时 scanout backing
// 生命周期由两方各自决定。
static DISPLAY_DEVICES: DeviceRegistry<dyn GraphicsDevice> = DeviceRegistry::new();
// OWNER: input adapter 按发现顺序的唯一发布点；index 即 evdev minor，缺失时 devfs event 号与 IRQ
// adapter 身份分裂。
static INPUT_DEVICES: DeviceRegistry<dyn InputDevice> = DeviceRegistry::new();
// OWNER: PCM playback adapter 的唯一发布点；缺失时两个 ALSA owner 可能控制同一 stream。
static PCM_OUTPUTS: DeviceRegistry<dyn PcmOutput> = DeviceRegistry::new();
// OWNER: entropy source 的唯一发布点；`getrandom` 与 `/dev/random` 共用首个 source。
static ENTROPY_SOURCES: DeviceRegistry<dyn EntropySource> = DeviceRegistry::new();
// OWNER: named byte-stream port 的唯一发布点；缺失时两个 byte-stream owner 可能竞争同一 SPICE
// channel。
static PORT_DEVICES: DeviceRegistry<dyn PortDevice> = DeviceRegistry::new();
// OWNER: console 设备的唯一发布点；TTY 按 `console=` 名称选择其一作为 `/dev/console`。
static CONSOLE_DEVICES: DeviceRegistry<dyn ConsoleDevice> = DeviceRegistry::new();
// OWNER: 经共享 `DRIVER_IO` deferred vector 发布 completion 的全部 adapter；adapter 构造成功时
// 自报。缺失某个源时其 completion 永远不被回收，同步 I/O waiter 永久睡眠。
static COMPLETION_SOURCES: DeviceRegistry<dyn CompletionSource> = DeviceRegistry::new();

/// 发布一个块设备。
///
/// # Errors
///
/// 注册表扩容失败时原样返回 adapter。
pub(crate) fn register_block_device(
    device: Arc<dyn BlockDevice>,
) -> Result<usize, Arc<dyn BlockDevice>> {
    BLOCK_DEVICES.register(device)
}

/// 第 `index` 个块设备。
pub(crate) fn block_device(index: usize) -> Option<Arc<dyn BlockDevice>> {
    BLOCK_DEVICES.get(index)
}

/// 发布一个 Ethernet 设备。
///
/// # Errors
///
/// 注册表扩容失败时原样返回 adapter。
pub(crate) fn register_network_device(
    device: Arc<dyn NetworkDevice>,
) -> Result<usize, Arc<dyn NetworkDevice>> {
    NETWORK_DEVICES.register(device)
}

/// 第 `index` 个 Ethernet 设备。
pub(crate) fn network_device(index: usize) -> Option<Arc<dyn NetworkDevice>> {
    NETWORK_DEVICES.get(index)
}

/// 发布一个 display adapter。
///
/// # Errors
///
/// 注册表扩容失败时原样返回 adapter。
pub(crate) fn register_display_device(
    device: Arc<dyn GraphicsDevice>,
) -> Result<usize, Arc<dyn GraphicsDevice>> {
    DISPLAY_DEVICES.register(device)
}

/// 第 `index` 个 display adapter。
pub(crate) fn display_device(index: usize) -> Option<Arc<dyn GraphicsDevice>> {
    DISPLAY_DEVICES.get(index)
}

/// 按发现顺序发布一个 input adapter；index 即 `/dev/input/eventN` 的 N。
///
/// # Errors
///
/// 注册表扩容失败时原样返回 adapter。
pub(crate) fn register_input_device(
    device: Arc<dyn InputDevice>,
) -> Result<usize, Arc<dyn InputDevice>> {
    INPUT_DEVICES.register(device)
}

/// 第 `index` 个 input adapter。
pub(crate) fn input_device(index: usize) -> Option<Arc<dyn InputDevice>> {
    INPUT_DEVICES.get(index)
}

/// 已发布 input adapter 数量。
pub(crate) fn input_device_count() -> usize {
    INPUT_DEVICES.count()
}

/// 发布一个 PCM playback adapter。
///
/// # Errors
///
/// 注册表扩容失败时原样返回 adapter。
pub(crate) fn register_pcm_output(device: Arc<dyn PcmOutput>) -> Result<usize, Arc<dyn PcmOutput>> {
    PCM_OUTPUTS.register(device)
}

/// 第 `index` 个 PCM playback adapter。
pub(crate) fn pcm_output(index: usize) -> Option<Arc<dyn PcmOutput>> {
    PCM_OUTPUTS.get(index)
}

/// 发布一个 entropy source。
///
/// # Errors
///
/// 注册表扩容失败时原样返回 adapter。
pub(crate) fn register_entropy_source(
    device: Arc<dyn EntropySource>,
) -> Result<usize, Arc<dyn EntropySource>> {
    ENTROPY_SOURCES.register(device)
}

/// 用首个 entropy source 完整初始化 caller-owned output。
///
/// # Errors
///
/// 没有 entropy source 或设备失败返回 unit error；不生成伪随机 fallback。
pub(crate) fn fill_entropy(bytes: &mut [MaybeUninit<u8>]) -> Result<(), ()> {
    ENTROPY_SOURCES.get(0).ok_or(())?.fill(bytes)
}

/// 发布一个 named byte-stream port。
///
/// # Errors
///
/// 注册表扩容失败时原样返回 adapter。
pub(crate) fn register_port_device(
    device: Arc<dyn PortDevice>,
) -> Result<usize, Arc<dyn PortDevice>> {
    PORT_DEVICES.register(device)
}

/// 第 `index` 个 named byte-stream port。
pub(crate) fn port_device(index: usize) -> Option<Arc<dyn PortDevice>> {
    PORT_DEVICES.get(index)
}

/// 发布一个 console 设备。
///
/// # Errors
///
/// 注册表扩容失败时原样返回 adapter。
pub(super) fn register_console_device(
    device: Arc<dyn ConsoleDevice>,
) -> Result<usize, Arc<dyn ConsoleDevice>> {
    CONSOLE_DEVICES.register(device)
}

/// 第 `index` 个 console 设备。
pub(crate) fn console_device(index: usize) -> Option<Arc<dyn ConsoleDevice>> {
    CONSOLE_DEVICES.get(index)
}

/// 登记一个经 `DRIVER_IO` vector 发布 completion 的 adapter。
///
/// # Errors
///
/// 注册表扩容失败时原样返回 adapter；调用方必须放弃该 adapter。
pub(super) fn register_completion_source(
    source: Arc<dyn CompletionSource>,
) -> Result<usize, Arc<dyn CompletionSource>> {
    COMPLETION_SOURCES.register(source)
}

/// 在 task/idle safe point 让每个 completion 源回收一批有界 completion。
///
/// # Returns
///
/// 任一源仍有 backlog 时返回 `true`，caller 必须重新发布 `DRIVER_IO`。
pub(crate) fn dispatch_io_completion_work() -> bool {
    let mut backlog = false;
    let mut index = 0;
    // 逐个取 Arc 后释放注册表锁再回收：回收会取得 adapter queue lock，不得嵌套在注册表锁内。
    while let Some(source) = COMPLETION_SOURCES.get(index) {
        backlog |= source.dispatch_completions();
        index += 1;
    }
    backlog
}
