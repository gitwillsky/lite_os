use alloc::sync::Arc;
use core::cell::Cell;

use smoltcp::{
    phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken},
    time::Instant,
    wire::IpEndpoint,
};

use super::device_error;
use crate::packet;
use device_model::network::{
    NetworkCompletion, NetworkDevice, NetworkError, NetworkReceiveFrame, NetworkStatistics,
    NetworkTransmit,
};

const ETHERNET_MTU: usize = 1514;
mod batch;
mod checksum;
mod ingress;
mod loopback;
use batch::ProtocolReceiveBatch;
use loopback::{LoopbackDevice, LoopbackRxToken, LoopbackTxToken};

/// 一个实际接口的phy mechanism；IP loopback与Ethernet NIC共用同一SocketSet/owner路径。
pub(super) enum ProtocolDevice {
    Ethernet(EthernetDevice),
    Loopback(LoopbackDevice),
}
impl ProtocolDevice {
    /// 领取一个有界 RX 批次；token 只借用设备，逐帧回调不会提前归还未消费槽。
    pub(super) fn receive_batch<'a, 'b>(
        &'a self,
        frames: &'b mut [Option<NetworkReceiveFrame<'a>>],
        facts: &'b mut [Option<ingress::IngressFacts>],
    ) -> Option<ProtocolReceiveBatch<'a, 'b>> {
        match self {
            Self::Ethernet(device) => device
                .receive_batch(frames, facts)
                .map(ProtocolReceiveBatch::Ethernet),
            Self::Loopback(device) => (!frames.is_empty() && device.can_receive()).then_some(
                ProtocolReceiveBatch::Loopback {
                    device,
                    remaining: frames
                        .len()
                        .min(device_model::network::RECEIVE_BATCH_CAPACITY),
                },
            ),
        }
    }
    pub(super) fn prepare_loopback(net_id: u64) -> Result<Self, network_protocol::SocketError> {
        LoopbackDevice::prepare(net_id).map(Self::Loopback)
    }
    /// 实际设备配置的L3 MTU；snapshot/packet准入/协议caps共用同一owner。
    pub(super) fn ip_mtu(&self) -> usize {
        self.capabilities().ip_mtu()
    }
    /// Native设备上下界准入后直接改变actual device；不复制到InterfaceState。
    pub(super) fn configure_mtu(&mut self, mtu: i32) -> Result<(), network_protocol::SocketError> {
        self.validate_mtu(mtu)?;
        match self {
            Self::Ethernet(device) => device.mtu = mtu as usize,
            Self::Loopback(device) => device.mtu = mtu as usize,
        }
        Ok(())
    }
    /// 无副作用地验证实际 adapter 的 MTU 范围，供配置事务在准备 FIB 前拒绝非法输入。
    pub(super) fn validate_mtu(&self, mtu: i32) -> Result<(), network_protocol::SocketError> {
        let (minimum, maximum) = match self {
            Self::Ethernet(_) => (68, ETHERNET_MTU as i32 - 14),
            Self::Loopback(_) => (0, loopback::LOOPBACK_MTU as i32),
        };
        if !(minimum..=maximum).contains(&mtu) {
            return Err(network_protocol::SocketError::Invalid);
        }
        Ok(())
    }
    pub(super) fn mac_address(&self) -> [u8; 6] {
        match self {
            Self::Ethernet(device) => device.mac_address(),
            Self::Loopback(_) => [0; 6],
        }
    }
    /// 底层驱动名；loopback 是协议栈内部设备，没有驱动（Linux 的 lo 也没有 `device`）。
    pub(super) fn driver(&self) -> Option<&str> {
        match self {
            Self::Ethernet(device) => Some(device.driver()),
            Self::Loopback(_) => None,
        }
    }
    /// 只捕获 NET 拥有的软件累计量；物理设备读取由快照调用者在 NET guard 外完成。
    pub(super) fn local_statistics(&self) -> NetworkStatistics {
        match self {
            Self::Ethernet(device) => device.local_statistics(),
            Self::Loopback(device) => device.statistics(),
        }
    }
    /// 实际routed reply预算/准入丢弃投影到同一device counter。
    pub(super) fn record_transmit_drop(&self) {
        match self {
            Self::Ethernet(device) => device
                .dropped_transmits
                .set(device.dropped_transmits.get().saturating_add(1)),
            Self::Loopback(device) => device.record_transmit_drop(),
        }
    }
    pub(super) fn take_ingress_reset(&self) -> Option<TcpReset> {
        match self {
            Self::Ethernet(device) => device.take_ingress_reset(),
            Self::Loopback(device) => device.take_ingress_reset(),
        }
    }
    pub(super) fn take_error(&self) -> Option<NetworkError> {
        match self {
            Self::Ethernet(device) => device.take_error(),
            Self::Loopback(_) => None,
        }
    }
    pub(super) fn poll_completions(
        &self,
        budget: usize,
    ) -> Result<NetworkCompletion, NetworkError> {
        match self {
            Self::Ethernet(device) => device.poll_completions(budget),
            Self::Loopback(_) => Ok(NetworkCompletion::default()),
        }
    }
    pub(super) fn finish_receive_batch(&self) -> Result<(), NetworkError> {
        match self {
            Self::Ethernet(device) => device.finish_receive_batch(),
            Self::Loopback(_) => Ok(()),
        }
    }
}
pub(super) enum ProtocolRxToken<'a> {
    Ethernet(EthernetRxToken<'a>),
    Loopback(LoopbackRxToken<'a>),
}
pub(super) enum ProtocolTxToken<'a> {
    Ethernet(EthernetTxToken<'a>),
    Loopback(LoopbackTxToken<'a>),
}
impl ProtocolRxToken<'_> {
    /// 领取输入时取得的路由地址；无效/未知协议不产生事实，原帧仍交给抓包和协议引擎。
    pub(super) fn addresses(&self) -> Option<(core::net::Ipv4Addr, core::net::Ipv4Addr)> {
        match self {
            Self::Ethernet(token) => token.facts.map(|facts| (facts.source, facts.destination)),
            Self::Loopback(token) => token.facts.map(|facts| (facts.source, facts.destination)),
        }
    }
}
impl RxToken for ProtocolRxToken<'_> {
    fn meta(&self) -> smoltcp::phy::PacketMeta {
        match self {
            Self::Ethernet(token) => token.meta(),
            Self::Loopback(token) => token.meta(),
        }
    }
    fn consume<R, F>(self, operation: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        match self {
            Self::Ethernet(token) => token.consume(operation),
            Self::Loopback(token) => token.consume(operation),
        }
    }
}
impl TxToken for ProtocolTxToken<'_> {
    fn set_meta(&mut self, meta: smoltcp::phy::PacketMeta) {
        match self {
            Self::Ethernet(token) => token.set_meta(meta),
            Self::Loopback(token) => token.set_meta(meta),
        }
    }
    fn consume<R, F>(self, length: usize, operation: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        match self {
            Self::Ethernet(token) => token.consume(length, operation),
            Self::Loopback(token) => token.consume(length, operation),
        }
    }
}
impl Device for ProtocolDevice {
    type RxToken<'a> = ProtocolRxToken<'a>;
    type TxToken<'a> = ProtocolTxToken<'a>;
    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let mut frames = [None];
        let mut facts = [None];
        self.receive_batch(&mut frames, &mut facts)?.next()?.ok()
    }
    fn transmit(&mut self, timestamp: Instant) -> Option<Self::TxToken<'_>> {
        match self {
            Self::Ethernet(device) => device.transmit(timestamp).map(ProtocolTxToken::Ethernet),
            Self::Loopback(device) => device.transmit(timestamp).map(ProtocolTxToken::Loopback),
        }
    }
    fn capabilities(&self) -> DeviceCapabilities {
        match self {
            Self::Ethernet(device) => device.capabilities(),
            Self::Loopback(device) => device.capabilities(),
        }
    }
}

/// 将 kernel Ethernet device seam 适配为 smoltcp token device。
pub(super) struct EthernetDevice {
    device: Arc<dyn NetworkDevice>,
    net_id: u64,
    interface_index: u32,
    // OWNER: 唯一配置L3 MTU；token/caps/snapshot只冻结或投影此值，硬件frame storage上限不变。
    mtu: usize,
    // OWNER: adapter 独占 callback 无法返回的首个 typed error；外层唯一 NetworkStack
    // mutex串行化全部访问，因此Cell不需要第二把锁。actual设备deferred诊断消费该状态；缺失
    // latch 会迫使 smoltcp Device 回调 panic 或静默丢错。
    pending_error: Cell<Option<NetworkError>>,
    // OWNER: 无可用TX reservation时的即时reply丢弃计数；RX仍必须继续，不能阻塞ACK。
    dropped_transmits: Cell<u64>,
    // OWNER: 此PHY实际callback错误与oversize已收帧丢弃；Native proc读取实际设备counter。
    // 外层NET guard独占，无新锁；不能让无法归属flow的硬件故障冒充任意socket SO_ERROR。
    received_errors: Cell<u64>,
    received_dropped: Cell<u64>,
    transmitted_errors: Cell<u64>,
    // OWNER: 仅保存当前一次 ingress callback 的 wire fact；不复制 socket 状态。
    // Closed transition 由 smoltcp 验证，fact 只区分 FIN 后的 RST 与正常终止。
    ingress_reset: Cell<Option<TcpReset>>,
    // OWNER: 主机栈持同一 NET guard，逐批轮转起点，防止繁忙 queue 0 饿死后续队列。
    // Cell 只让存续的批借用与独立 queue cursor 共存，不增加共享并发 owner 或第二把锁。
    receive_queue: Cell<u16>,
    completion_queue: Cell<u16>,
}

impl EthernetDevice {
    /// 创建不复制硬件状态的协议栈 adapter。
    ///
    /// # Parameters
    ///
    /// - `device`: DTB 选中的唯一 Ethernet device。
    ///
    /// # Returns
    ///
    /// 只持共享设备 Arc 的 adapter。
    pub(super) fn new(device: Arc<dyn NetworkDevice>, net_id: u64, interface_index: u32) -> Self {
        Self {
            device,
            net_id,
            interface_index,
            mtu: ETHERNET_MTU - 14,
            pending_error: Cell::new(None),
            dropped_transmits: Cell::new(0),
            received_errors: Cell::new(0),
            received_dropped: Cell::new(0),
            transmitted_errors: Cell::new(0),
            ingress_reset: Cell::new(None),
            receive_queue: Cell::new(0),
            completion_queue: Cell::new(0),
        }
    }

    pub(super) fn take_ingress_reset(&self) -> Option<TcpReset> {
        self.ingress_reset.replace(None)
    }

    pub(super) fn network_device(&self) -> Arc<dyn NetworkDevice> {
        self.device.clone()
    }

    pub(super) fn mac_address(&self) -> [u8; 6] {
        self.device.mac_address()
    }

    pub(super) fn driver(&self) -> &str {
        self.device.driver()
    }

    pub(super) fn local_statistics(&self) -> NetworkStatistics {
        NetworkStatistics {
            transmitted_dropped: self.dropped_transmits.get(),
            received_errors: self.received_errors.get(),
            received_dropped: self.received_dropped.get(),
            transmitted_errors: self.transmitted_errors.get(),
            ..Default::default()
        }
    }

    /// 有界回收设备 TX completion。
    ///
    /// # Parameters
    ///
    /// - `budget`: 本轮最多回收的 descriptor head 数。
    ///
    /// # Returns
    ///
    /// backlog 与 capacity transition。
    ///
    /// # Errors
    ///
    /// 设备或 used ring 损坏时返回错误。
    pub(super) fn poll_completions(
        &self,
        budget: usize,
    ) -> Result<NetworkCompletion, NetworkError> {
        let count = self.device.queue_count();
        let start = self.completion_queue.get();
        self.completion_queue.set((start + 1) % count);
        let mut total = NetworkCompletion::default();
        for offset in 0..count {
            let queue = ((u32::from(start) + u32::from(offset)) % u32::from(count)) as u16;
            let done = self
                .device
                .poll_completions(queue, budget - total.completed)
                .inspect_err(|error| self.record_transmit_error(*error))?;
            if done.completed > budget - total.completed {
                return Err(NetworkError::Device);
            }
            total.completed += done.completed;
            total.backlog |= done.backlog;
            total.transmit_became_available |= done.transmit_became_available;
        }
        Ok(total)
    }

    /// 把本轮重新发布的 RX buffers 一次通知给设备。
    ///
    /// # Returns
    ///
    /// 成功或 transport 错误。
    pub(super) fn finish_receive_batch(&self) -> Result<(), NetworkError> {
        for queue in 0..self.device.queue_count() {
            self.device
                .finish_receive_batch(queue)
                .inspect_err(|error| self.record_receive_error(*error))?;
        }
        Ok(())
    }

    /// 在actual设备deferred诊断phase消费callback首个adapter error。
    ///
    /// # Returns
    ///
    /// pending error；没有错误返回 `None`。
    pub(super) fn take_error(&self) -> Option<NetworkError> {
        self.pending_error.replace(None)
    }

    fn record_error(&self, error: NetworkError) {
        if error == NetworkError::WouldBlock {
            return;
        }
        if self.pending_error.get().is_none() {
            self.pending_error.set(Some(error));
        }
    }

    fn record_receive_error(&self, error: NetworkError) {
        if error == NetworkError::WouldBlock {
            return;
        }
        self.record_error(error);
        self.received_errors
            .set(self.received_errors.get().saturating_add(1));
        if error == NetworkError::FrameTooLarge {
            self.received_dropped
                .set(self.received_dropped.get().saturating_add(1));
        }
    }

    fn record_transmit_error(&self, error: NetworkError) {
        if error == NetworkError::WouldBlock {
            return;
        }
        self.record_error(error);
        self.transmitted_errors
            .set(self.transmitted_errors.get().saturating_add(1));
    }
}

/// 一个实际输入帧中的 RST 地址；只用于已经发生的真实 Closed transition。
#[derive(Clone, Copy)]
pub(super) struct TcpReset {
    local: IpEndpoint,
    peer: IpEndpoint,
}

impl TcpReset {
    fn from_transport(
        source: core::net::Ipv4Addr,
        destination: core::net::Ipv4Addr,
        bytes: &[u8],
    ) -> Option<Self> {
        // 此处只记录实际输入的 RST tuple；选项、checksum 与状态转换仍由唯一协议引擎裁决。
        let tcp = packet_wire::tcp::TcpPrefix::parse(bytes).ok()?;
        (tcp.flags() & packet_wire::tcp::RST != 0).then(|| Self {
            local: IpEndpoint::new(destination.into(), tcp.destination_port()),
            peer: IpEndpoint::new(source.into(), tcp.source_port()),
        })
    }

    pub(super) fn matches(
        self,
        local: network_protocol::InetAddress,
        peer: network_protocol::InetAddress,
    ) -> bool {
        self.local == IpEndpoint::new(local.address.into(), local.port)
            && self.peer == IpEndpoint::new(peer.address.into(), peer.port)
    }
}

/// 已完成接收并由一次 smoltcp ingress callback 消费的 frame token。
pub(super) struct EthernetRxToken<'a> {
    net_id: u64,
    interface_index: u32,
    mac: [u8; 6],
    frame: NetworkReceiveFrame<'a>,
    facts: Option<ingress::IngressFacts>,
    received_errors: &'a Cell<u64>,
    pending_error: &'a Cell<Option<NetworkError>>,
    ingress_reset: &'a Cell<Option<TcpReset>>,
}

impl RxToken for EthernetRxToken<'_> {
    fn meta(&self) -> smoltcp::phy::PacketMeta {
        let mut metadata = smoltcp::phy::PacketMeta::default();
        metadata.rx_checksum_valid = self.frame.checksum_valid();
        metadata
    }
    fn consume<R, F>(self, operation: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        let frame = &self.frame;
        self.ingress_reset
            .set(self.facts.and_then(|facts| facts.reset));
        packet::deliver(
            self.net_id,
            self.interface_index,
            self.mac,
            frame,
            frame.checksum_valid(),
        );
        let result = operation(frame);
        if let Err(error) = self.frame.release() {
            self.received_errors
                .set(self.received_errors.get().saturating_add(1));
            if self.pending_error.get().is_none() {
                self.pending_error.set(Some(error));
            }
        }
        result
    }
}

/// 持有唯一 adapter reservation，并把异步 submit error 锁存回 device owner 的 TX token。
pub(super) struct EthernetTxToken<'a> {
    net_id: u64,
    interface_index: u32,
    reservation: Option<NetworkTransmit<'a>>,
    // FLAG: 本次 emitter 已明确省略 transport checksum；缺失会把未完成的包误当普通帧发送。
    checksum: bool,
    maximum_frame_length: usize,
    dropped_transmits: &'a Cell<u64>,
    transmitted_errors: &'a Cell<u64>,
    pending_error: &'a Cell<Option<NetworkError>>,
}

impl TxToken for EthernetTxToken<'_> {
    fn set_meta(&mut self, meta: smoltcp::phy::PacketMeta) {
        self.checksum = meta.tx_checksum;
    }
    fn consume<R, F>(self, length: usize, operation: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        assert!(
            length <= self.maximum_frame_length,
            "smoltcp TX exceeds actual Ethernet MTU"
        );
        let render = |frame: &mut [u8]| {
            let result = operation(frame);
            let checksum = if self.checksum {
                checksum::prepare(frame)
            } else {
                Ok(None)
            };
            if length >= 14 {
                packet::deliver_outgoing(
                    self.net_id,
                    self.interface_index,
                    1,
                    frame[..14].try_into().unwrap(),
                    &frame[14..],
                    matches!(checksum, Ok(Some(_))),
                    None,
                );
            }
            (result, checksum)
        };
        let (result, error) = match self.reservation {
            Some(mut reservation) if length <= reservation.capacity() => {
                let frame = &mut reservation.buffer_mut()[..length];
                // 保留 codec 的 reserved/padding 零初始化前提，只清实际范围；直接写预留槽，不另复制帧。
                frame.fill(0);
                let (result, checksum) = render(frame);
                (
                    result,
                    checksum
                        .and_then(|checksum| reservation.submit(length, checksum, None))
                        .err(),
                )
            }
            reservation => {
                let error = reservation.as_ref().map(|_| NetworkError::FrameTooLarge);
                drop(reservation);
                // 无发送槽也要执行同一协议推进（不能因 TX 拥塞停止 RX/ACK）；此帧只丢弃，不发布 DMA。
                let mut discarded = [0u8; ETHERNET_MTU];
                let (result, _) = render(&mut discarded[..length]);
                if error.is_none() {
                    self.dropped_transmits
                        .set(self.dropped_transmits.get().saturating_add(1));
                }
                (result, error)
            }
        };
        if let Some(error) = error {
            self.transmitted_errors
                .set(self.transmitted_errors.get().saturating_add(1));
            self.dropped_transmits
                .set(self.dropped_transmits.get().saturating_add(1));
            if self.pending_error.get().is_none() {
                self.pending_error.set(Some(error));
            }
        }
        result
    }
}

impl Device for EthernetDevice {
    type RxToken<'a> = EthernetRxToken<'a>;
    type TxToken<'a> = EthernetTxToken<'a>;

    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let mut frames = [None];
        let mut facts = [None];
        self.receive_batch(&mut frames, &mut facts)?.next()?.ok()
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        match device_error::classify_optional(self.device.reserve_transmit(0), |error| {
            *error == NetworkError::WouldBlock
        }) {
            Ok(reservation) => reservation.map(|reservation| EthernetTxToken {
                net_id: self.net_id,
                interface_index: self.interface_index,
                reservation: Some(reservation),
                checksum: false,
                maximum_frame_length: self.mtu + 14,
                dropped_transmits: &self.dropped_transmits,
                transmitted_errors: &self.transmitted_errors,
                pending_error: &self.pending_error,
            }),
            Err(error) => {
                self.record_transmit_error(error);
                None
            }
        }
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut capabilities = DeviceCapabilities::default();
        capabilities.medium = Medium::Ethernet;
        capabilities.max_transmission_unit = self.mtu + 14;
        if self.device.transmit_capabilities().checksum {
            capabilities.checksum.tcp = smoltcp::phy::Checksum::Rx;
            capabilities.checksum.udp = smoltcp::phy::Checksum::Rx;
        }
        capabilities
    }
}
