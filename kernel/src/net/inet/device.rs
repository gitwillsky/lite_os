use alloc::sync::Arc;
use core::cell::Cell;

use smoltcp::{
    phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken},
    time::Instant,
    wire::{EthernetFrame, EthernetProtocol, IpEndpoint, IpProtocol, Ipv4Packet, TcpPacket},
};

use super::super::packet;
use super::device_error;
use crate::devices::model::network::{
    NetworkCompletion, NetworkDevice, NetworkError, NetworkStatistics, NetworkTransmit,
};

const ETHERNET_MTU: usize = 1514;
const RECEIVE_CAPACITY: usize = 2048;
mod loopback;
use loopback::{LoopbackDevice, LoopbackRxToken, LoopbackTxToken};

/// 一个实际接口的phy mechanism；IP loopback与Ethernet NIC共用同一SocketSet/owner路径。
pub(super) enum ProtocolDevice {
    Ethernet(EthernetDevice),
    Loopback(LoopbackDevice),
}
impl ProtocolDevice {
    pub(super) fn prepare_loopback(net_id: u64) -> Result<Self, crate::net::SocketError> {
        LoopbackDevice::prepare(net_id).map(Self::Loopback)
    }
    /// 实际设备配置的L3 MTU；snapshot/packet准入/协议caps共用同一owner。
    pub(super) fn ip_mtu(&self) -> usize {
        self.capabilities().ip_mtu()
    }
    /// Native设备上下界准入后直接改变actual device；不复制到InterfaceState。
    pub(super) fn configure_mtu(&mut self, mtu: i32) -> Result<(), crate::net::SocketError> {
        let (minimum, maximum) = match self {
            Self::Ethernet(_) => (68, ETHERNET_MTU as i32 - 14),
            Self::Loopback(_) => (0, loopback::LOOPBACK_MTU as i32),
        };
        if !(minimum..=maximum).contains(&mtu) {
            return Err(crate::net::SocketError::Invalid);
        }
        match self {
            Self::Ethernet(device) => device.mtu = mtu as usize,
            Self::Loopback(device) => device.mtu = mtu as usize,
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
    pub(super) fn driver(&self) -> Option<&'static str> {
        match self {
            Self::Ethernet(device) => Some(device.driver()),
            Self::Loopback(_) => None,
        }
    }
    pub(super) fn statistics(&self) -> NetworkStatistics {
        match self {
            Self::Ethernet(device) => device.statistics(),
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
#[allow(
    clippy::large_enum_variant,
    reason = "RX token 内联持有有界物理帧；每包堆分配会引入 OOM 丢包与热路径分配"
)]
pub(super) enum ProtocolRxToken<'a> {
    Ethernet(EthernetRxToken<'a>),
    Loopback(LoopbackRxToken<'a>),
}
pub(super) enum ProtocolTxToken<'a> {
    Ethernet(EthernetTxToken<'a>),
    Loopback(LoopbackTxToken<'a>),
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
    fn receive(&mut self, timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        match self {
            Self::Ethernet(device) => device
                .receive(timestamp)
                .map(|(rx, tx)| (ProtocolRxToken::Ethernet(rx), ProtocolTxToken::Ethernet(tx))),
            Self::Loopback(device) => device
                .receive(timestamp)
                .map(|(rx, tx)| (ProtocolRxToken::Loopback(rx), ProtocolTxToken::Loopback(tx))),
        }
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

    pub(super) fn driver(&self) -> &'static str {
        self.device.driver()
    }

    pub(super) fn statistics(&self) -> NetworkStatistics {
        let mut statistics = self.device.statistics();
        statistics.transmitted_dropped = statistics
            .transmitted_dropped
            .saturating_add(self.dropped_transmits.get());
        statistics.received_errors = statistics
            .received_errors
            .saturating_add(self.received_errors.get());
        statistics.received_dropped = statistics
            .received_dropped
            .saturating_add(self.received_dropped.get());
        statistics.transmitted_errors = statistics
            .transmitted_errors
            .saturating_add(self.transmitted_errors.get());
        statistics
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
        self.device
            .poll_completions(budget)
            .inspect_err(|error| self.record_transmit_error(*error))
    }

    /// 把本轮重新发布的 RX buffers 一次通知给设备。
    ///
    /// # Returns
    ///
    /// 成功或 transport 错误。
    pub(super) fn finish_receive_batch(&self) -> Result<(), NetworkError> {
        self.device
            .finish_receive_batch()
            .inspect_err(|error| self.record_receive_error(*error))
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
    fn from_frame(frame: &[u8]) -> Option<Self> {
        let ethernet = EthernetFrame::new_checked(frame).ok()?;
        if ethernet.ethertype() != EthernetProtocol::Ipv4 {
            return None;
        }
        Self::from_ip_packet(ethernet.payload())
    }
    fn from_ip_packet(packet: &[u8]) -> Option<Self> {
        let ip = Ipv4Packet::new_checked(packet).ok()?;
        if ip.next_header() != IpProtocol::Tcp || ip.more_frags() || ip.frag_offset() != 0 {
            return None;
        }
        let tcp = TcpPacket::new_checked(ip.payload()).ok()?;
        tcp.rst().then(|| Self {
            local: IpEndpoint::new(ip.dst_addr().into(), tcp.dst_port()),
            peer: IpEndpoint::new(ip.src_addr().into(), tcp.src_port()),
        })
    }

    pub(super) fn matches(
        self,
        local: crate::net::InetAddress,
        peer: crate::net::InetAddress,
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
    frame: [u8; RECEIVE_CAPACITY],
    length: usize,
    ingress_reset: &'a Cell<Option<TcpReset>>,
}

impl RxToken for EthernetRxToken<'_> {
    fn consume<R, F>(self, operation: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        let frame = &self.frame[..self.length];
        self.ingress_reset.set(TcpReset::from_frame(frame));
        packet::deliver(self.net_id, self.interface_index, self.mac, frame);
        operation(frame)
    }
}

/// 持有唯一 adapter reservation，并把异步 submit error 锁存回 device owner 的 TX token。
pub(super) struct EthernetTxToken<'a> {
    net_id: u64,
    interface_index: u32,
    reservation: Option<NetworkTransmit>,
    maximum_frame_length: usize,
    dropped_transmits: &'a Cell<u64>,
    transmitted_errors: &'a Cell<u64>,
    pending_error: &'a Cell<Option<NetworkError>>,
}

impl TxToken for EthernetTxToken<'_> {
    fn consume<R, F>(self, length: usize, operation: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        assert!(
            length <= self.maximum_frame_length,
            "smoltcp TX exceeds actual Ethernet MTU"
        );
        let mut frame = [0u8; ETHERNET_MTU];
        let result = operation(&mut frame[..length]);
        if length >= 14 {
            packet::deliver_outgoing(
                self.net_id,
                self.interface_index,
                1,
                frame[..14].try_into().unwrap(),
                &frame[14..length],
                u16::from_be_bytes([frame[12], frame[13]]),
                None,
            );
        }
        if let Some(reservation) = self.reservation {
            if let Err(error) = reservation.submit(&frame[..length]) {
                self.transmitted_errors
                    .set(self.transmitted_errors.get().saturating_add(1));
                self.dropped_transmits
                    .set(self.dropped_transmits.get().saturating_add(1));
                if self.pending_error.get().is_none() {
                    self.pending_error.set(Some(error));
                }
            }
        } else {
            self.dropped_transmits
                .set(self.dropped_transmits.get().saturating_add(1));
        }
        result
    }
}

impl Device for EthernetDevice {
    type RxToken<'a> = EthernetRxToken<'a>;
    type TxToken<'a> = EthernetTxToken<'a>;

    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        self.ingress_reset.set(None);
        let reservation = match device_error::classify_optional(
            NetworkTransmit::reserve(self.device.clone()),
            |error| *error == NetworkError::WouldBlock,
        ) {
            // 库要求receive同时提供不可失败的reply token；TX拥塞不应阻塞RX/ACK。
            // 无reservation时token只执行同一codec并丢弃reply，已排队TX仍由transmit背压。
            Ok(reservation) => reservation,
            Err(error) => {
                self.record_transmit_error(error);
                return None;
            }
        };
        let mut frame = [0u8; RECEIVE_CAPACITY];
        match device_error::classify_optional(self.device.receive(&mut frame), |error| {
            *error == NetworkError::WouldBlock
        }) {
            Ok(Some(length)) => Some((
                EthernetRxToken {
                    net_id: self.net_id,
                    interface_index: self.interface_index,
                    mac: self.device.mac_address(),
                    frame,
                    length,
                    ingress_reset: &self.ingress_reset,
                },
                EthernetTxToken {
                    net_id: self.net_id,
                    interface_index: self.interface_index,
                    reservation,
                    maximum_frame_length: self.mtu + 14,
                    dropped_transmits: &self.dropped_transmits,
                    transmitted_errors: &self.transmitted_errors,
                    pending_error: &self.pending_error,
                },
            )),
            Ok(None) => None,
            Err(error) => {
                self.record_receive_error(error);
                None
            }
        }
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        match device_error::classify_optional(
            NetworkTransmit::reserve(self.device.clone()),
            |error| *error == NetworkError::WouldBlock,
        ) {
            Ok(reservation) => reservation.map(|reservation| EthernetTxToken {
                net_id: self.net_id,
                interface_index: self.interface_index,
                reservation: Some(reservation),
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
        capabilities
    }
}
