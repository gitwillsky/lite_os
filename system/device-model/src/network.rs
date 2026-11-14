mod offload;
pub use offload::{
    NetworkQueueConfiguration, NetworkTcpSegmentation, NetworkTcpVersion,
    NetworkTransmitCapabilities,
};
mod checksum;
pub use checksum::NetworkChecksum;
mod frame;
pub use frame::{
    NetworkReceiveBuffer, NetworkReceiveFrame, NetworkReceiveOwner, NetworkReceivePayload,
    NetworkTransmit, NetworkTransmitData, ReceiveChecksum, ReceivePreparation,
};
pub use packet_wire::batch::NetworkPacket as ReceivePacket;
mod rss;
pub use rss::{NetworkRssCapabilities, NetworkRssConfiguration};

/// 按位依次为 IPv4/TCPv4/UDPv4、IPv6/TCPv6/UDPv6、IPv6-EX/TCPv6-EX/UDPv6-EX。
/// 不含隧道内层散列；其他网卡驱动须从自身硬件位映射到这些语义。
pub const RECEIVE_HASH_TYPES: u32 = 0x1ff;

/// 一轮共享队列领取的有界批容量；固定栈窗口，不随积压分配或占用全部 RX 槽。
pub const RECEIVE_BATCH_CAPACITY: usize = 32;

/// network device seam 的错误分类；协议栈不得感知具体 VirtIO adapter。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkError {
    /// 设备暂时没有已完成的接收帧。
    WouldBlock,
    /// 请求的硬件配置无效。
    InvalidConfiguration,
    /// 共享缓冲或会话元数据无法分配。
    NoMemory,
    /// 中断槽或设备地址空间不足；准备阶段失败，没有改变当前路由。
    NoResources,
    /// 帧超过设备或调用方 buffer 能表达的 MTU。
    FrameTooLarge,
    /// transport、queue 或设备返回了不可恢复错误。
    Device,
    /// 设备会话已关闭（驱动宿主终止或网卡被移除）；使用者撤下该接口。
    Disconnected,
}

/// 当前以太网物理链路，与接口管理开关独立。
#[derive(Debug, Clone, Copy, Default)]
pub struct NetworkLink {
    pub carrier: bool,
    /// 传输会话观察到的载波代际。直连硬件提供者没有传输代际时置零。
    pub carrier_epoch: u32,
    /// 设备报告的兆比特每秒；未报告时为 None。
    pub speed_mbps: Option<u32>,
}

/// 唯一 Ethernet adapter owner 投影的累计 counters。
#[derive(Debug, Clone, Copy, Default)]
pub struct NetworkStatistics {
    pub received_bytes: u64,
    pub received_packets: u64,
    pub received_errors: u64,
    pub received_dropped: u64,
    pub transmitted_bytes: u64,
    pub transmitted_packets: u64,
    pub transmitted_errors: u64,
    /// 软件/设备TX拥塞丢包，与成功提交计数分开。
    pub transmitted_dropped: u64,
}

impl NetworkStatistics {
    /// 将另一队列的累计计数饱和汇入网卡快照；只用于低频统计，不复制队列 owner 状态。
    pub fn accumulate(&mut self, other: Self) {
        self.received_bytes = self.received_bytes.saturating_add(other.received_bytes);
        self.received_packets = self.received_packets.saturating_add(other.received_packets);
        self.received_errors = self.received_errors.saturating_add(other.received_errors);
        self.received_dropped = self.received_dropped.saturating_add(other.received_dropped);
        self.transmitted_bytes = self
            .transmitted_bytes
            .saturating_add(other.transmitted_bytes);
        self.transmitted_packets = self
            .transmitted_packets
            .saturating_add(other.transmitted_packets);
        self.transmitted_errors = self
            .transmitted_errors
            .saturating_add(other.transmitted_errors);
        self.transmitted_dropped = self
            .transmitted_dropped
            .saturating_add(other.transmitted_dropped);
    }
}

/// 一次有界 device completion drain 的结果。
#[derive(Debug, Clone, Copy, Default)]
pub struct NetworkCompletion {
    /// 本轮实际回收的 TX descriptor head 数，用于跨队列共享同一完成预算。
    pub completed: usize,
    /// 队列仍有待推进的工作，调用方须重新投递。
    pub backlog: bool,
    /// TX 容量从零变为非零，阻塞的 packet writer 需要被唤醒。
    pub transmit_became_available: bool,
}

/// 设备队列线程已发表、尚未被协议栈消费的进度。
#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
pub struct NetworkDeviceProgress {
    /// 至少一个队列从无发送槽变为可写。
    pub transmit_ready: bool,
    /// 接收方向待处理的故障。
    pub receive_error: Option<NetworkError>,
    /// 发送方向待处理的故障。
    pub transmit_error: Option<NetworkError>,
}

/// 面向 Ethernet 协议栈的唯一 network device seam。
///
/// 收发方法的 `queue` 必须位于 `0..queue_count()`；同一设备的队列可并发调用。
/// `reservation` 只属于创建它的 queue，不能跨队列提交/取消；每队列分别绑定活动通知。
pub trait NetworkDevice: Send + Sync {
    /// 当前活动 RX/TX queue pair 数；编号为 0..queue_count，实例寿命内固定且非零。
    fn queue_count(&self) -> u16;

    /// 是否提供每个 queue pair 独立的中断 CPU 路由；来自实际硬件/adapter 能力。
    fn supports_queue_affinity(&self) -> bool;

    /// 将本 pair 的 RX/TX 中断路由到同一个在线逻辑 CPU，不改变其他 pair 或配置向量。
    /// # Errors
    /// 不支持、队列/CPU 无效返回 InvalidConfiguration；准备失败且未改变路由返回 NoMemory/NoResources。
    /// 中途设备/配置失败停止整个设备并返回 Device，不能留下仍在运行的半配置 pair。
    fn set_queue_affinity(&self, queue: u16, cpu: u32) -> Result<(), NetworkError>;

    /// 已协商的 TX checksum/TSO 与实际槽容量；普通帧仍按原内容发送。
    fn transmit_capabilities(&self) -> NetworkTransmitCapabilities;

    /// 绑定接收/发送完成时调用的活动通知；绑定前到达的完成由绑定时的一次通知补偿。
    fn bind_activity(&self, queue: u16, activity: crate::Activity);
    /// 登记独立链路变化通知并补发一次；不得用普通收发活动冒充链路变化。
    /// 静态链路实现无需后续通知；可变链路在状态已可读后通知，不传递第二份状态。
    /// 回调允许重入只读 link；不得持有会被 link 重取的设备/租约锁。
    fn bind_link_activity(&self, activity: crate::Activity);

    /// 返回设备出厂 MAC 地址。
    ///
    /// # Returns
    ///
    /// 六字节 unicast Ethernet address。
    fn mac_address(&self) -> [u8; 6];

    /// 驱动名：Linux sysfs `/sys/class/net/<if>/device/uevent` 的 `DRIVER=` 值（如 `virtio_net`）。
    fn driver(&self) -> &str;

    /// 读取驱动已发表的链路事实，不等待驱动控制 RPC，不领取或改写数据队列。
    /// 物理设备无法读取或会话已退出时返回错误；变化由独立链路通知发布。
    fn link(&self) -> Result<NetworkLink, NetworkError>;

    /// 从 `start` 起循环选择一个有待交付帧的队列，非阻塞地领取一批 RX。
    /// `output` 开始必须全为 None。成功时返回队列号和填入的非空前缀长度，无包时返回 None。
    /// 返回数不超过 output.len 与 RECEIVE_BATCH_CAPACITY，每帧有独立的不可复制借用，不复制 payload。
    /// # Errors
    /// 输出含已有 token、起始队列无效或设备/协议失败。错误前可能已经填入部分 token，调用者必须全部归还，不能消费其 payload；
    /// 设备错误锁存终止。不得在持有设备本地队列锁时 Drop 这些 token，否则归还会重入同一锁。
    fn receive_batch<'a>(
        &'a self,
        start: u16,
        output: &mut [Option<NetworkReceiveFrame<'a>>],
    ) -> Result<Option<(u16, usize)>, NetworkError>;

    /// 预留并返回唯一的可写 payload；无容量返回 WouldBlock，设备失败返回错误。
    fn reserve_transmit(&self, queue: u16) -> Result<NetworkTransmit<'_>, NetworkError>;

    /// 把对应 TX reservation 交给发送队列。成功后不能取消或访问 payload。
    /// # Safety
    /// 只能由唯一 TX token 消费该身份，所有 CPU 内容引用必须已结束，length 不超过容量。
    /// # Errors
    /// 接管时的设备或格式错误。失败须保留尚未交出的 reservation，或在终止后拒绝复用。
    unsafe fn submit_transmit(
        &self,
        queue: u16,
        slot: u16,
        ticket: u64,
        length: usize,
        data: NetworkTransmitData,
    ) -> Result<(), NetworkError>;

    /// 取消尚未交出的 TX token；发送队列持有的槽不能提前归还。
    /// # Safety
    /// 只能消费唯一 token，且所有 payload 引用必须已结束；不可取消其他 token 的身份。
    unsafe fn cancel_transmit(&self, queue: u16, slot: u16, ticket: u64);

    /// 读取当前是否可以立即预留 TX slot。
    ///
    /// # Returns
    ///
    /// 至少一个 free slot 时返回 `true`。
    fn transmit_available(&self, queue: u16) -> bool;

    /// 由设备的唯一协议所有者消费队列已发表的可写与故障通知。
    /// # Returns
    /// 自上次消费后的合并通知。发送可写与两个方向的故障分别报告。
    fn take_progress(&self) -> NetworkDeviceProgress;

    /// 读取与 queue completion 同一 owner 更新的累计 counters。
    ///
    /// # Returns
    ///
    /// 自设备初始化后的 RX/TX byte 与 packet 数。
    fn statistics(&self) -> NetworkStatistics;
}

/// 已由硬件完成、尚未归还的接收槽；内容位于该队列共享 VMO 的 payload 范围。
#[derive(Debug, Clone, Copy)]
pub struct NetworkReceive {
    /// 当前 queue pair 内的槽号。
    pub slot: u16,
    /// 不含设备 header 的字节数。
    pub length: usize,
    /// 设备已验证 transport checksum，或驱动已按设备 seed 补全。
    pub checksum_valid: bool,
}

/// 单队列共享存储的授权与布局；两份 VMO 由硬件提供者固定并保活。
/// 客户端只在持有对应槽的 CPU ownership 时读写 payload，不能把映射存在当成所有权。
pub struct NetworkQueueMemory {
    /// NIC 写入的 RX 存储；receive 完成到 release_receive 之间归客户端。
    pub receive: native_runtime::Vmo,
    /// 客户端写入的 TX 存储；完成交还后至下次批量提交前归客户端。
    pub transmit: native_runtime::Vmo,
    /// RX 槽数。
    pub receive_slots: u16,
    /// TX 槽数。
    pub transmit_slots: u16,
    /// 每个 RX 槽的字节跨度。
    pub receive_stride: usize,
    /// 每个 TX 槽的字节跨度。
    pub transmit_stride: usize,
    /// 槽开头的设备私有 header，客户端不得访问。
    pub header_bytes: usize,
    /// 最大 TX payload 长度。
    pub transmit_limit: usize,
}

/// 一次批量提交中的共享发送槽；payload 已由提交者写完，接受后不能再访问。
#[derive(Clone, Copy, Debug)]
pub struct NetworkTransmitDescriptor {
    /// 当前 pair 内的槽号。
    pub slot: u16,
    /// 不含设备 header 的字节数。
    pub length: usize,
    /// 显式的 partial checksum 意图；None 必须保持所有原始 payload 字节。
    pub checksum: Option<NetworkChecksum>,
    /// 显式 TCP 大包分段；必须同时提供 checksum 区域与 seed。
    pub segmentation: Option<NetworkTcpSegmentation>,
}

/// 物理网卡的创建能力；公布硬件上限，队列数由使用者而非 CPU 探测策略决定。
pub trait NetworkController: Send + Sync {
    /// 是否支持每个活动 pair 独立选择 IRQ 目标 CPU；不能用共享 IRQ 假装独立路由。
    fn supports_queue_affinity(&self) -> bool;
    /// 设备可配置的最大收发 pair 数，非零。多个 pair 必须支持自动或可配置的接收分流。
    fn maximum_queue_pairs(&self) -> u16;
    /// 物理 TX checksum/TSO 与容量上限；本次实际容量由 OPEN 明确请求。
    fn transmit_capabilities(&self) -> NetworkTransmitCapabilities;
    /// RSS 硬件能力；不支持可配置散列时为 None。
    fn receive_scaling(&self) -> Option<NetworkRssCapabilities>;
    /// 六字节 MAC。
    fn mac_address(&self) -> [u8; 6];
    /// 驱动自行提供的名称。
    fn driver(&self) -> &str;
    /// 按 configuration 独占创建精确的 pair 数与 TX 容量；旧会话撤销 DMA 后才允许再次打开。
    /// # Errors
    /// 零/超限请求、仍有硬件租约、配置/分配或设备错误时失败，不悄悄减少数量。
    fn open(
        self: alloc::sync::Arc<Self>,
        configuration: NetworkQueueConfiguration,
    ) -> Result<alloc::sync::Arc<dyn NetworkProvider>, NetworkError>;
}

/// 实际网络硬件提供者：只交换共享槽的描述，不实现客户端的 CPU 帧适配。
/// 每个已接受 RX 完成必须归还一次；TX 槽交给 DMA owner 后只能由硬件完成或同步 stop 释放。
pub trait NetworkProvider: Send + Sync {
    /// 同步完成本 pair 的 RX/TX 目标 CPU 配置；中途失败必须停止数据面，错误语义见 NetworkDevice。
    /// # Errors
    /// 不支持/无效返回 InvalidConfiguration，无变化的准备失败返回 NoMemory/NoResources，中途失败返回 Device。
    fn set_queue_affinity(&self, queue: u16, cpu: u32) -> Result<(), NetworkError>;
    /// 活动 pair 数，实例寿命内固定且非零。
    fn queue_count(&self) -> u16;
    /// 为 queue 登记活动通知，补偿登记前可能到达的完成。
    fn bind_activity(&self, queue: u16, activity: crate::Activity);
    /// 独立链路通知，约束同 NetworkDevice::bind_link_activity；实现必须覆盖绑定前变化，并在本地锁外回调。
    fn bind_link_activity(&self, activity: crate::Activity);
    /// 六字节出厂 MAC。
    fn mac_address(&self) -> [u8; 6];
    /// 提供者拥有的驱动名。
    fn driver(&self) -> &str;
    /// 当前链路；读取失败返回设备错误。
    fn link(&self) -> Result<NetworkLink, NetworkError>;
    /// 同步替换 RSS 参数；只在实际硬件 ACK 后成功，失败不得继续访问未完成的控制 DMA。
    /// # Errors
    /// 不支持或配置无效时返回 InvalidConfiguration；控制/硬件失败返回 Device，分配失败返回 NoMemory。
    fn set_receive_scaling(&self, config: &NetworkRssConfiguration) -> Result<(), NetworkError>;
    /// 授权共享 VMO 与真实布局；无效队列、句柄复制或分配失败返回错误。
    fn memory(&self, queue: u16) -> Result<NetworkQueueMemory, NetworkError>;
    /// 有界领取 RX 完成，返回写入 output 的数量；已交出的槽在 release_receive_batch 前归 CPU。
    /// 空队列返回零，设备错误返回失败并由调用者停止数据面。
    fn receive_batch(
        &self,
        queue: u16,
        output: &mut [NetworkReceive],
    ) -> Result<usize, NetworkError>;
    /// 将 `queue` 的 `slots` 整批归还设备。每个槽的 CPU 借用必须已经结束，且只归还一次。
    /// 空批不做工作，成功时整批可供设备接收。设备错误时调用者必须 stop，不得重试归还。
    fn release_receive_batch(&self, queue: u16, slots: &[u16]) -> Result<(), NetworkError>;
    /// 批量提交空闲槽，返回已接受的前缀长度；零表示暂时无容量，未接受后缀仍归提交者。
    /// 错误表示本数据面不可用，调用者必须 stop，不能把错误当作尚未接受。
    fn transmit_batch(
        &self,
        queue: u16,
        input: &[NetworkTransmitDescriptor],
    ) -> Result<usize, NetworkError>;
    /// 回收至多 output.len 个完成，写入真实槽号；所有输出都已结束 DMA 并归还硬件槽。
    fn reap_transmit(
        &self,
        queue: u16,
        output: &mut [u16],
    ) -> Result<NetworkCompletion, NetworkError>;
    /// 同步停止全部 DMA；返回后才能发表会话 closed，复位失败必须保活并 fail-stop。
    fn stop(&self);
    /// 低频汇总真实队列累计量。
    fn statistics(&self) -> NetworkStatistics;
}

#[cfg(test)]
mod tests {
    use super::{
        NetworkDevice, NetworkDeviceProgress, NetworkError, NetworkLink, NetworkReceiveFrame,
        NetworkStatistics, NetworkTransmit,
    };
    use alloc::sync::Arc;
    use native_runtime::sync::Mutex;

    struct InjectedDevice {
        submit_error: bool,
        cancellations: Mutex<usize>,
        slot: Mutex<(alloc::vec::Vec<u8>, bool)>,
    }

    impl NetworkDevice for InjectedDevice {
        fn supports_queue_affinity(&self) -> bool {
            false
        }
        fn set_queue_affinity(&self, _: u16, _: u32) -> Result<(), NetworkError> {
            Err(NetworkError::InvalidConfiguration)
        }
        fn queue_count(&self) -> u16 {
            1
        }
        fn transmit_capabilities(&self) -> super::NetworkTransmitCapabilities {
            super::NetworkTransmitCapabilities {
                checksum: false,
                tcp_ipv4: false,
                tcp_ipv6: false,
                tcp_ecn: false,
                maximum_frame_length: 1514,
                maximum_buffer_length: 1514,
            }
        }
        fn bind_activity(&self, _: u16, _: crate::Activity) {}
        fn bind_link_activity(&self, activity: crate::Activity) {
            activity();
        }
        fn mac_address(&self) -> [u8; 6] {
            [0; 6]
        }

        fn driver(&self) -> &str {
            "injected"
        }

        fn link(&self) -> Result<NetworkLink, NetworkError> {
            Ok(NetworkLink::default())
        }

        fn receive_batch<'a>(
            &'a self,
            start: u16,
            output: &mut [Option<NetworkReceiveFrame<'a>>],
        ) -> Result<Option<(u16, usize)>, NetworkError> {
            if start >= self.queue_count() || output.iter().any(Option::is_some) {
                return Err(NetworkError::InvalidConfiguration);
            }
            Ok(None)
        }
        fn reserve_transmit(&self, queue: u16) -> Result<NetworkTransmit<'_>, NetworkError> {
            let mut slot = self.slot.lock();
            if slot.1 {
                return Err(NetworkError::WouldBlock);
            }
            slot.1 = true;
            let address = slot.0.as_mut_ptr() as usize;
            // SAFETY: fixture 的 Vec 独立分配且 device 保活，reserved 时不 resize；唯一 token 结束后才归还。
            Ok(unsafe { NetworkTransmit::new(self, queue, 7, 1, address, slot.0.len()) })
        }
        // SAFETY: 本 fixture 不产生 RX token，路径不可达，不持有任何接收内存。
        // SAFETY: 实际 token 已结束内容引用；fixture 只更新其独占槽状态，没有异步 DMA。
        unsafe fn submit_transmit(
            &self,
            _: u16,
            _: u16,
            _: u64,
            _: usize,
            _: super::NetworkTransmitData,
        ) -> Result<(), NetworkError> {
            if self.submit_error {
                Err(NetworkError::Device)
            } else {
                self.slot.lock().1 = false;
                Ok(())
            }
        }
        // SAFETY: 实际 token 的 Drop 恰好取消一次；此处只归还独占状态并记录次数。
        unsafe fn cancel_transmit(&self, _: u16, _: u16, _: u64) {
            self.slot.lock().1 = false;
            *self.cancellations.lock() += 1;
        }

        fn transmit_available(&self, _queue: u16) -> bool {
            true
        }

        fn take_progress(&self) -> NetworkDeviceProgress {
            NetworkDeviceProgress::default()
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
            slot: Mutex::new((alloc::vec![0], false)),
        });
        let device: Arc<dyn NetworkDevice> = adapter.clone();

        assert_eq!(
            device.reserve_transmit(0).unwrap().submit(
                1,
                None,
                Some(super::NetworkTcpSegmentation {
                    version: super::NetworkTcpVersion::Ipv4,
                    maximum_segment_size: 1460,
                    ecn: false,
                })
            ),
            Err(NetworkError::InvalidConfiguration)
        );
        assert_eq!(*adapter.cancellations.lock(), 1);
        assert_eq!(
            device.reserve_transmit(0).unwrap().submit(1, None, None),
            Err(NetworkError::Device)
        );
        assert_eq!(*adapter.cancellations.lock(), 2);
    }

    #[test]
    fn successful_submit_transfers_reservation_without_cancel() {
        let adapter = Arc::new(InjectedDevice {
            submit_error: false,
            cancellations: Mutex::new(0),
            slot: Mutex::new((alloc::vec![0], false)),
        });
        let device: Arc<dyn NetworkDevice> = adapter.clone();

        let mut frame = device.reserve_transmit(0).unwrap();
        frame.buffer_mut()[0] = 0xa5;
        assert_eq!(frame.submit(1, None, None), Ok(()));
        assert_eq!(adapter.slot.lock().0[0], 0xa5);
        assert_eq!(*adapter.cancellations.lock(), 0);
    }
}
