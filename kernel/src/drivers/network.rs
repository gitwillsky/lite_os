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

/// @description 一次有界 device completion drain 的结果。
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct NetworkCompletion {
    /// 尚有 used entry 未回收，调用方必须重新投递 network softirq。
    pub(crate) backlog: bool,
    /// TX 容量从零变为非零，阻塞的 packet writer 需要被唤醒。
    pub(crate) transmit_became_available: bool,
}

/// @description 不可复制的 Ethernet TX slot reservation。
///
/// token 在提交前丢弃会归还 slot；成功提交后 descriptor 只能由 used-ring
/// completion 归还。这使 smoltcp 获得 TxToken 到填充 frame 的窗口内不会被
/// AF_PACKET sender 抢走最后一个 slot。
pub(crate) struct NetworkTransmit {
    device: Arc<dyn NetworkDevice>,
    reservation: Option<u16>,
}

impl NetworkTransmit {
    /// @description 从适配器的固定 TX pool 中预留一个 slot。
    ///
    /// @param device DTB 选中的唯一 Ethernet adapter。
    /// @return 拥有唯一 reservation 的 token。
    /// @errors pool 已满返回 `WouldBlock`；设备状态损坏返回 `Device`。
    pub(crate) fn reserve(device: Arc<dyn NetworkDevice>) -> Result<Self, NetworkError> {
        let reservation = device.reserve_transmit()?;
        Ok(Self {
            device,
            reservation: Some(reservation),
        })
    }

    /// @description 把完整 Ethernet frame 复制到预留 DMA slot 并发布 descriptor。
    ///
    /// @param frame 不含 VirtIO header 的 Ethernet frame。
    /// @return descriptor 成功发布返回 unit；实际 DMA 完成由 softirq 回收。
    /// @errors frame 过大或 transport 失败返回对应错误。
    pub(crate) fn submit(mut self, frame: &[u8]) -> Result<(), NetworkError> {
        let reservation = self
            .reservation
            .as_ref()
            .copied()
            .expect("network transmit reservation consumed twice");
        self.device.submit_transmit(reservation, frame)?;
        // 只有 adapter 已成功把 reservation 转为 in-flight owner 后才解除 Drop rollback。
        // 若提前 take，任一可恢复构造错误都会泄漏固定 TX slot 并最终永久 EAGAIN。
        self.reservation.take();
        Ok(())
    }
}

impl Drop for NetworkTransmit {
    fn drop(&mut self) {
        if let Some(reservation) = self.reservation.take() {
            self.device.cancel_transmit(reservation);
        }
    }
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

    /// @description 从固定 TX pool 中预留一个 slot。
    ///
    /// @return adapter-private slot ID。
    /// @errors pool 已满返回 `WouldBlock`；owner 状态损坏返回 `Device`。
    fn reserve_transmit(&self) -> Result<u16, NetworkError>;

    /// @description 把已预留 slot 转换为唯一 in-flight descriptor membership。
    ///
    /// @param reservation `reserve_transmit` 返回且尚未消费的 slot ID。
    /// @param frame 不含 VirtIO header 的 Ethernet frame。
    /// @return descriptor 已发布返回 unit。
    /// @errors frame 过长或 publication 前 transport 失败返回对应错误；返回错误时
    /// reservation 仍由 caller 持有，随后必须且只能取消一次。
    fn submit_transmit(&self, reservation: u16, frame: &[u8]) -> Result<(), NetworkError>;

    /// @description 取消尚未发布的 TX reservation。
    ///
    /// @param reservation 由同一 adapter 发布的 slot ID。
    /// @return 无返回值；重复取消或取消 in-flight slot 会 fail-stop。
    fn cancel_transmit(&self, reservation: u16);

    /// @description 读取当前是否可以立即预留 TX slot。
    ///
    /// @return 至少一个 free slot 时返回 `true`。
    fn transmit_available(&self) -> bool;

    /// @description 有界回收 TX used-ring completion。
    ///
    /// @param budget 本轮最多回收的 descriptor head 数。
    /// @return backlog 与 TX capacity transition。
    /// @errors used ring 或 descriptor owner 损坏返回 `Device`。
    fn poll_completions(&self, budget: usize) -> Result<NetworkCompletion, NetworkError>;

    /// @description 一轮 RX drain 结束后批量通知设备新的 available buffers。
    ///
    /// @return 无 pending repost 时为空操作。
    /// @errors MMIO transport 失败返回 `Device`。
    fn finish_receive_batch(&self) -> Result<(), NetworkError>;

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

/// @description 发布本 CPU 的 network deferred work，由 user-return/idle safe point 消费。
/// @return 无返回值。
#[cfg(not(test))]
pub(crate) fn request_poll() {
    crate::cpu::raise_deferred(crate::cpu::DeferredWork::Network);
}

#[cfg(test)]
mod tests {
    use super::{
        NetworkCompletion, NetworkDevice, NetworkError, NetworkStatistics, NetworkTransmit,
    };
    use alloc::sync::Arc;
    use spin::Mutex;

    struct InjectedDevice {
        submit_error: bool,
        cancellations: Mutex<usize>,
    }

    impl NetworkDevice for InjectedDevice {
        fn mac_address(&self) -> [u8; 6] {
            [0; 6]
        }

        fn receive(&self, _frame: &mut [u8]) -> Result<usize, NetworkError> {
            Err(NetworkError::WouldBlock)
        }

        fn reserve_transmit(&self) -> Result<u16, NetworkError> {
            Ok(7)
        }

        fn submit_transmit(&self, _reservation: u16, _frame: &[u8]) -> Result<(), NetworkError> {
            if self.submit_error {
                Err(NetworkError::Device)
            } else {
                Ok(())
            }
        }

        fn cancel_transmit(&self, _reservation: u16) {
            *self.cancellations.lock() += 1;
        }

        fn transmit_available(&self) -> bool {
            true
        }

        fn poll_completions(&self, _budget: usize) -> Result<NetworkCompletion, NetworkError> {
            Ok(NetworkCompletion::default())
        }

        fn finish_receive_batch(&self) -> Result<(), NetworkError> {
            Ok(())
        }

        fn statistics(&self) -> NetworkStatistics {
            NetworkStatistics::default()
        }
    }

    #[test]
    fn device_failure_rolls_back_transmit_reservation_once() {
        let adapter = Arc::new(InjectedDevice {
            submit_error: true,
            cancellations: Mutex::new(0),
        });
        let device: Arc<dyn NetworkDevice> = adapter.clone();

        assert_eq!(
            NetworkTransmit::reserve(device).unwrap().submit(&[]),
            Err(NetworkError::Device)
        );
        assert_eq!(*adapter.cancellations.lock(), 1);
    }

    #[test]
    fn successful_submit_transfers_reservation_without_cancel() {
        let adapter = Arc::new(InjectedDevice {
            submit_error: false,
            cancellations: Mutex::new(0),
        });
        let device: Arc<dyn NetworkDevice> = adapter.clone();

        assert_eq!(
            NetworkTransmit::reserve(device).unwrap().submit(&[]),
            Ok(())
        );
        assert_eq!(*adapter.cancellations.lock(), 0);
    }
}
