//! 设备类注册表：platform 按发现顺序发布同一 seam 的多个 adapter。
//!
//! 每个设备类（console、network、display、input、PCM output、entropy、named port）与共享
//! driver-I/O completion 源各有一个只追加的注册表；index 是 adapter 的稳定 identity。选择哪个
//! 实例是消费领域的策略，注册表不区分“主设备”。

use alloc::sync::Arc;
use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicUsize, Ordering};
use spin::Once;

use super::{
    EntropySource, GraphicsDevice, InputDevice, PcmOutput, PortDevice,
    io_completion::CompletionSource, network::NetworkDevice,
};
use crate::hal::registry::AppendOnlyRegistry;

// OWNER: Ethernet adapter 的唯一发布点；协议栈与 AF_PACKET 按 index 绑定同一接口，缺失时两者
// 可能持有不同 adapter 而分裂 MAC 与 RX ownership。
static NETWORK_DEVICES: AppendOnlyRegistry<dyn NetworkDevice> = AppendOnlyRegistry::new();
// OWNER: display adapter 的唯一发布点；IRQ handler 与 DRM 持有同一 Arc，缺失时 scanout backing
// 生命周期由两方各自决定。
static DISPLAY_DEVICES: AppendOnlyRegistry<dyn GraphicsDevice> = AppendOnlyRegistry::new();
// OWNER: input adapter 按发现顺序的唯一发布点；index 即 evdev minor，缺失时 devfs event 号与 IRQ
// adapter 身份分裂。
static INPUT_DEVICES: AppendOnlyRegistry<dyn InputDevice> = AppendOnlyRegistry::new();
// OWNER: PCM playback adapter 的唯一发布点；缺失时两个 ALSA owner 可能控制同一 stream。
static PCM_OUTPUTS: AppendOnlyRegistry<dyn PcmOutput> = AppendOnlyRegistry::new();
// OWNER: entropy source 的唯一发布点；`getrandom` 与 `/dev/random` 共用首个 source。
static ENTROPY_SOURCES: AppendOnlyRegistry<dyn EntropySource> = AppendOnlyRegistry::new();
// OWNER: named byte-stream port 的唯一发布点；缺失时两个 byte-stream owner 可能竞争同一 SPICE
// channel。
static PORT_DEVICES: AppendOnlyRegistry<dyn PortDevice> = AppendOnlyRegistry::new();
/// 共享 `DRIVER_IO` vector 的 completion 源上限：每个 virtio-blk、virtio-rng 与 virtio-sound adapter 各占一项。
const COMPLETION_SOURCE_CAPACITY: usize = 16;

// OWNER: 经共享 `DRIVER_IO` deferred vector 发布 completion 的全部 adapter；adapter 构造成功时
// 自报。只追加的 `Once` 槽位使每次 completion dispatch 无锁读取；缺失某个源时其 completion
// 永远不被回收，同步 I/O waiter 永久睡眠。
static COMPLETION_SOURCES: [Once<Arc<dyn CompletionSource>>; COMPLETION_SOURCE_CAPACITY] =
    [const { Once::new() }; COMPLETION_SOURCE_CAPACITY];
// OWNER: 下一个未分配的 completion 源槽位；`fetch_add` 使并发注册取得不同槽位。
static NEXT_COMPLETION_SOURCE: AtomicUsize = AtomicUsize::new(0);

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

/// 登记一个经 `DRIVER_IO` vector 发布 completion 的 adapter。
///
/// # Errors
///
/// 槽位已满时原样返回 adapter；调用方必须放弃该 adapter。
pub(crate) fn register_completion_source(
    source: Arc<dyn CompletionSource>,
) -> Result<usize, Arc<dyn CompletionSource>> {
    let index = NEXT_COMPLETION_SOURCE.fetch_add(1, Ordering::Relaxed);
    let Some(slot) = COMPLETION_SOURCES.get(index) else {
        return Err(source);
    };
    slot.call_once(|| source);
    Ok(index)
}

/// 在 task/idle safe point 让每个 completion 源回收一批有界 completion。
///
/// # Returns
///
/// 任一源仍有 backlog 时返回 `true`，caller 必须重新发布 `DRIVER_IO`。
pub(crate) fn dispatch_io_completion_work() -> bool {
    // 全部槽位逐个检查而不在首个空槽停止：并发注册可能先分配槽位、稍后才发布。
    COMPLETION_SOURCES
        .iter()
        .filter_map(Once::get)
        .fold(false, |backlog, source| {
            source.dispatch_completions() | backlog
        })
}
