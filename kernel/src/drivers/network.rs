use alloc::sync::Arc;
use spin::{Mutex, Once};

/// @description network device seam 的错误分类；协议栈不得感知具体 VirtIO adapter。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NetworkError {
    /// 设备暂时没有已完成的接收帧。
    WouldBlock,
    /// 帧超过设备或调用方 buffer 能表达的 MTU。
    FrameTooLarge,
    /// transport、queue 或设备返回了不可恢复错误。
    Device,
}

/// @description 唯一 Ethernet adapter owner 投影的累计 counters。
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct NetworkStatistics {
    pub(crate) received_bytes: u64,
    pub(crate) received_packets: u64,
    pub(crate) transmitted_bytes: u64,
    pub(crate) transmitted_packets: u64,
}

/// @description 面向 Ethernet 协议栈的唯一 network device seam。
pub(crate) trait NetworkDevice: Send + Sync {
    /// @description 返回设备出厂 MAC 地址。
    ///
    /// @return 六字节 unicast Ethernet address。
    fn mac_address(&self) -> [u8; 6];

    /// @description 非阻塞取出一个完整 Ethernet frame。
    ///
    /// @param frame kernel-owned 接收缓冲区。
    /// @return 帧长度；当前无包返回 `WouldBlock`，损坏或过长返回对应错误。
    fn receive(&self, frame: &mut [u8]) -> Result<usize, NetworkError>;

    /// @description 同步提交一个完整 Ethernet frame。
    ///
    /// @param frame 不含 VirtIO header 的 Ethernet frame。
    /// @return DMA 完成返回成功；过长或 transport 失败返回对应错误。
    fn transmit(&self, frame: &[u8]) -> Result<(), NetworkError>;

    /// @description 读取与 queue completion 同一 owner 更新的累计 counters。
    ///
    /// @return 自设备初始化后的 RX/TX byte 与 packet 数。
    fn statistics(&self) -> NetworkStatistics;
}

// OWNER: driver network seam uniquely owns the DTB-selected Ethernet device. A second binding
// would split MAC identity and RX ownership between protocol-stack instances.
static PRIMARY_NETWORK_DEVICE: Once<Mutex<Option<Arc<dyn NetworkDevice>>>> = Once::new();

fn binding() -> &'static Mutex<Option<Arc<dyn NetworkDevice>>> {
    PRIMARY_NETWORK_DEVICE.call_once(|| Mutex::new(None))
}

/// @description 发布 DTB 扫描选中的唯一 Ethernet device。
///
/// @param device 已完成 feature negotiation 与 queue 初始化的设备。
/// @return 首次注册成功；已有设备返回原 Arc，调用方必须拒绝双 owner。
pub(super) fn register_network_device(
    device: Arc<dyn NetworkDevice>,
) -> Result<(), Arc<dyn NetworkDevice>> {
    let mut slot = binding().lock();
    if slot.is_some() {
        return Err(device);
    }
    *slot = Some(device);
    Ok(())
}

/// @description 获取协议栈使用的唯一 Ethernet device。
///
/// @return 已注册设备；平台没有 network device 时返回 `None`。
pub(crate) fn network_device() -> Option<Arc<dyn NetworkDevice>> {
    binding().lock().clone()
}
