use alloc::sync::Arc;
use core::{
    cell::{Cell, RefMut},
    ops::Deref,
};

use crate::packet;
use device_model::network::{
    NetworkDevice, NetworkError, NetworkReceiveFrame, NetworkStatistics, NetworkTransmit,
    ReceiveChecksum,
};
use packet_wire::batch::IngressIp;

const ETHERNET_MTU: usize = 1514;
mod batch;
mod checksum;
mod loopback;
mod transmit;
mod vlan;
use batch::ProtocolReceiveBatch;
use loopback::{LoopbackDevice, LoopbackRxToken, LoopbackTxToken};
pub(in crate::inet) use transmit::ReceivedPayload;
pub(in crate::inet) use vlan::VlanLink;

/// 一个实际接口的phy mechanism；IP loopback与Ethernet NIC共用同一传输owner路径。
pub(super) enum ProtocolDevice {
    Ethernet(EthernetDevice),
    Loopback(LoopbackDevice),
}
impl ProtocolDevice {
    /// 向网卡提交完整的交换帧，保留背压和适配层错误。
    /// VLAN 标签和最小帧填充占用网卡帧容量，均不计入端口的载荷 MTU。
    pub(super) fn send_bridged(
        &self,
        header: packet_wire::ethernet::EthernetHeader<'_>,
        payload: &[u8],
    ) -> Result<(), NetworkError> {
        let Self::Ethernet(device) = self else {
            return Err(NetworkError::InvalidConfiguration);
        };
        let Some(nic) = device.nic() else {
            return Err(NetworkError::InvalidConfiguration);
        };
        let length = (header.length() + payload.len()).max(60);
        if payload.len() > device.mtu
            || length > nic.transmit_capabilities().maximum_frame_length as usize
        {
            return Err(NetworkError::FrameTooLarge);
        }
        let mut token = device.reserve(|_| 0)?;
        if !matches!(&token.reservation, Some(EthernetReservation::Physical(slot)) if slot.capacity() >= length)
        {
            device.record_transmit_error(NetworkError::FrameTooLarge);
            return Err(NetworkError::FrameTooLarge);
        }
        token.maximum_frame_length = length;
        token.consume_committed(length, |frame| {
            let at = header
                .emit(frame)
                .expect("validated bridge header fits output");
            let (body, padding) = frame[at..].split_at_mut(payload.len());
            body.copy_from_slice(payload);
            padding.fill(0);
            true
        })
    }
    /// 领取一个有界 RX 批次；token 只借用设备，逐帧回调不会提前归还未消费槽。
    pub(super) fn receive_batch<'a, 'b>(
        &'a self,
        frames: &'b mut [Option<NetworkReceiveFrame<'a>>],
        host_ingress: bool,
    ) -> Option<ProtocolReceiveBatch<'a, 'b>> {
        match self {
            Self::Ethernet(device) => {
                if let Some(link) = device.vlan() {
                    (!frames.is_empty() && host_ingress && link.receive_pending()).then_some(
                        ProtocolReceiveBatch::Vlan {
                            device,
                            link,
                            remaining: frames.len(),
                        },
                    )
                } else {
                    device
                        .receive_batch(frames, host_ingress)
                        .map(ProtocolReceiveBatch::Ethernet)
                }
            }
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
    pub(super) fn prepare_loopback(
        net_id: u64,
        packets: &Arc<packet::PacketRegistry>,
    ) -> Result<Self, network_protocol::SocketError> {
        LoopbackDevice::prepare(net_id, packets.clone()).map(Self::Loopback)
    }
    /// 实际设备配置的 L3 MTU，由接口快照和报文准入共用。
    pub(super) fn ip_mtu(&self) -> usize {
        match self {
            Self::Ethernet(device) => device.mtu,
            Self::Loopback(device) => device.mtu,
        }
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
            Self::Ethernet(device) => device.nic().map(NetworkDevice::driver),
            Self::Loopback(_) => None,
        }
    }

    /// 逻辑端口以与物理接口相同的 Ethernet 链路语义接入 IP。
    pub(super) fn vlan(&self) -> Option<&VlanLink> {
        match self {
            Self::Ethernet(device) => device.vlan(),
            Self::Loopback(_) => None,
        }
    }

    /// 逻辑发送窗口重新可用，通知由 NET 轮次在锁外统一交付。
    pub(super) fn notify_vlan_tx_ready(&self) {
        if let Self::Ethernet(device) = self {
            packet::publish_transmit_ready(&device.packets, device.interface_index);
        }
    }

    /// 退休本接口的抓包绑定。就绪通知由本域工作线程在协议锁外发送。
    pub(super) fn retire_packet_binding(&self) {
        if let Self::Ethernet(device) = self {
            packet::publish_interface_removed(&device.packets, device.interface_index);
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
            Self::Ethernet(device) => device.record_transmit_drop(),
            Self::Loopback(device) => device.record_transmit_drop(),
        }
    }
    /// 实际物理接口的 IP 拒绝计入原入口 RX drop；本机重组和转发共用，不伪造 socket 错误。
    pub(super) fn record_receive_drop(&self) {
        if let Self::Ethernet(device) = self {
            device
                .received_dropped
                .set(device.received_dropped.get().saturating_add(1));
        }
    }
    pub(super) fn take_error(&self) -> Option<NetworkError> {
        match self {
            Self::Ethernet(device) => device.take_error(),
            Self::Loopback(_) => None,
        }
    }
    pub(super) fn take_transmit_ready(&self) -> bool {
        match self {
            Self::Ethernet(device) => device.take_transmit_ready(),
            Self::Loopback(_) => false,
        }
    }
}
pub(super) enum ProtocolRxToken<'a> {
    Ethernet(EthernetRxToken<'a>),
    Loopback(LoopbackRxToken<'a>),
    // 唯一重组 owner 的完整包移交；不再持 NIC loan，也不重复产生物理抓包记录。
    Reassembled(super::reassembly::Frame),
}
pub(super) enum ProtocolTxToken<'a> {
    Ethernet(EthernetTxToken<'a>),
    Loopback(LoopbackTxToken<'a>),
}

/// 核对物理单播目标和合法的以太网源，适用于原片与带原链路头的重组包。
pub(super) fn physical_unicast(frame: &[u8], mac: [u8; 6]) -> bool {
    frame.get(..6) == Some(mac.as_slice())
        && frame
            .get(6..12)
            .is_some_and(|source| source[0] & 1 == 0 && source != [0; 6])
}

impl ProtocolRxToken<'_> {
    /// 物理 RX 借用的队列。虚拟链路由协议 owner 使用控制队列。
    pub(super) fn queue(&self) -> u16 {
        match self {
            Self::Ethernet(token) => match &token.frame {
                EthernetReceive::Physical(frame) => frame.queue(),
                EthernetReceive::Vlan { .. } => 0,
            },
            Self::Loopback(_) | Self::Reassembled(_) => 0,
        }
    }
    pub(super) fn with_frame<R>(&self, inspect: impl FnOnce(&[u8]) -> R) -> R {
        match self {
            Self::Ethernet(token) => inspect(&token.frame),
            Self::Loopback(token) => token.with_frame(inspect),
            Self::Reassembled(frame) => inspect(&frame.bytes),
        }
    }
    /// 消费原始帧并提供同一 RX 借用的 IP 解析结果。回环和重组仍提供各自的原始字节。
    pub(super) fn consume_with_ip<R>(
        self,
        operation: impl FnOnce(
            &[u8],
            Option<packet_wire::batch::NetworkPacket<'_>>,
            Option<&NetworkReceiveFrame<'_>>,
        ) -> R,
    ) -> R {
        match self {
            Self::Ethernet(token) => token.consume_frame(|frame| {
                match frame {
                    EthernetReceive::Physical(frame) => {
                        operation(frame, frame.direct_ip(), Some(frame))
                    }
                    EthernetReceive::Vlan { .. } => {
                        operation(frame, packet_wire::batch::NetworkPacket::direct_ip(frame), None)
                    }
                }
            }),
            token => token.consume(|frame| operation(frame, None, None)),
        }
    }
    pub(super) fn reassembled(&self) -> bool {
        matches!(self, Self::Reassembled(_))
    }
    /// 只有同一帧已有结构事实时，才可排除它需要重组检查。
    pub(super) fn may_need_reassembly(&self) -> bool {
        let facts = match self {
            Self::Ethernet(token) => token.frame.ingress_ip(),
            Self::Loopback(token) => token.facts,
            Self::Reassembled(_) => return false,
        };
        facts.is_none_or(IngressIp::may_need_reassembly)
    }
    /// 取出可重新提交到转发出口的物理分片，归一化报文仍可交本机。
    pub(super) fn take_fragments(&mut self) -> Option<super::reassembly::Fragments> {
        match self {
            Self::Reassembled(frame) => frame.fragments.take(),
            _ => None,
        }
    }
    /// 返回发往本机单播 MAC 的真实 IPv4 源/目的；其余返回 None，广播、ARP、VLAN 与回环仍交给各自入口。
    pub(super) fn transit_addresses(&self) -> Option<(core::net::Ipv4Addr, core::net::Ipv4Addr)> {
        let Self::Ethernet(token) = self else {
            return None;
        };
        let addresses = token.frame.ingress_ip()?.addresses()?;
        physical_unicast(&token.frame, token.mac).then_some(addresses)
    }
    /// 同一物理帧的传输层校验，软件结果只供共享同一字节范围的转发入口。
    pub(super) fn transport_checksum(&self) -> ReceiveChecksum {
        match self {
            Self::Ethernet(EthernetRxToken {
                frame: EthernetReceive::Physical(frame),
                ..
            }) => frame.transport_checksum(),
            _ => ReceiveChecksum::Unchecked,
        }
    }

    /// 同一帧原始地址的预查找值，只来自实际物理队列。
    pub(super) fn prefix_probe(&self) -> Option<super::route::ReceivePrefix> {
        let Self::Ethernet(EthernetRxToken {
            frame: EthernetReceive::Physical(frame),
            ..
        }) = self
        else {
            return None;
        };
        let annotation = frame.annotation();
        let probe = annotation[super::protocol_owner::RECEIVE_PREFIX];
        if probe == 0 {
            return None;
        }
        let ip = frame.ingress_ip()?;
        let source_probe = annotation[super::protocol_owner::RECEIVE_SOURCE_PREFIX];
        Some(super::route::ReceivePrefix::new(
            ip.destination(),
            probe,
            ip.addresses().map(|(source, _)| (source, source_probe)),
        ))
    }

    /// 返回同一物理帧的原始 TCP/UDP 查找散列。设备重新装配到另一域时旧值无效。
    pub(super) fn receive_flow(&self, domain: u64) -> Option<super::forward::ReceiveFlow> {
        let Self::Ethernet(EthernetRxToken {
            frame: EthernetReceive::Physical(frame),
            ..
        }) = self
        else {
            return None;
        };
        let annotation = frame.annotation();
        if annotation[super::protocol_owner::RECEIVE_FLOW_DOMAIN] != domain {
            return None;
        }
        Some(super::forward::ReceiveFlow {
            hash: annotation[super::protocol_owner::RECEIVE_FLOW_HASH],
            observation: annotation[super::protocol_owner::RECEIVE_FLOW_OBSERVATION]
                .try_into()
                .expect("three observation words"),
        })
    }
    /// ARP 全部由原生入口处理，包括 framing 畸形的 ARP。
    pub(super) fn arp(&self) -> bool {
        matches!(self, Self::Ethernet(token) if token.frame.get(12..14) == Some(&[0x08, 0x06]))
    }
    pub(super) fn ethernet_source(&self) -> Option<[u8; 6]> {
        let Self::Ethernet(token) = self else {
            return None;
        };
        token.frame.get(6..12)?.try_into().ok()
    }
    pub(super) fn ipv6(&self) -> bool {
        match self {
            Self::Ethernet(token) => token.frame.ingress_ip().is_some_and(IngressIp::is_ipv6),
            Self::Loopback(token) => token.ipv6(),
            Self::Reassembled(frame) => frame.ip()[0] >> 4 == 6,
        }
    }
    /// 领取输入时取得的路由地址；无效/未知协议不产生事实，原帧仍交给抓包和协议引擎。
    pub(super) fn addresses(&self) -> Option<(core::net::Ipv4Addr, core::net::Ipv4Addr)> {
        match self {
            Self::Ethernet(token) => token.frame.ingress_ip().and_then(IngressIp::addresses),
            Self::Loopback(token) => token.facts.and_then(IngressIp::addresses),
            Self::Reassembled(frame) => packet_wire::ipv4::Ipv4Packet::parse_header(frame.ip())
                .ok()
                .map(|ip| (ip.source(), ip.destination())),
        }
    }
}
impl ProtocolRxToken<'_> {
    /// 逻辑交付接口仅随回环与重组保留，物理收包使用所属接口。
    pub(super) fn interface_index(&self) -> Option<u32> {
        match self {
            Self::Ethernet(_) => None,
            Self::Loopback(token) => token.interface_index(),
            Self::Reassembled(frame) => Some(frame.interface),
        }
    }

    /// 只有原始物理帧能携带完整传输层校验和结果。
    pub(super) fn checksum_valid(&self) -> bool {
        matches!(self, Self::Ethernet(token) if token.frame.checksum_valid())
    }

    /// 消费原帧，归还唯一 RX 借用。
    pub(super) fn consume<R>(self, operation: impl FnOnce(&[u8]) -> R) -> R {
        match self {
            Self::Ethernet(token) => token.consume(operation),
            Self::Loopback(token) => token.consume(operation),
            Self::Reassembled(frame) => operation(&frame.bytes),
        }
    }
}
impl ProtocolTxToken<'_> {
    /// 只在实际发送窗口编码。编码成功且设备提交完成后返回成功。
    /// `protocol` 是回环抓包的链路协议，物理发送由编码器填写完整链路头。
    /// 编码器填满窗口后返回 true，返回 false 时取消提交。
    /// # Errors
    /// 编码失败、超出设备帧上限、背压或硬件提交失败。
    pub(super) fn consume_committed(
        self,
        length: usize,
        protocol: u16,
        operation: impl FnOnce(&mut [u8]) -> bool,
    ) -> Result<(), NetworkError> {
        match self {
            Self::Ethernet(token) => token.consume_committed(length, operation),
            Self::Loopback(token) => token.consume_ip(length, protocol, operation),
        }
    }

    /// 回环携带逻辑交付接口，物理发送仅接受明确的传输层校验和卸载请求。
    pub(super) fn set_delivery(&mut self, interface: u32, checksum: bool) {
        match self {
            Self::Ethernet(token) => token.checksum = checksum,
            Self::Loopback(token) => token.set_interface(interface),
        }
    }
}

/// Ethernet 接口的实际收发来源。
enum EthernetLink {
    Physical(Arc<dyn NetworkDevice>),
    Vlan(VlanLink),
}

pub(super) struct EthernetDevice {
    // 先于网卡引用退休；已在运行的队列工作只会访问旧会话。
    queues: crate::worker::DeviceQueues,
    link: EthernetLink,
    net_id: u64,
    packets: Arc<packet::PacketRegistry>,
    interface_index: u32,
    // OWNER: 唯一配置L3 MTU；token/caps/snapshot只冻结或投影此值，硬件frame storage上限不变。
    mtu: usize,
    // OWNER: 记录首个未报告的设备错误，由域轮询诊断消费。外层 NET guard 串行化访问。
    pending_error: Cell<Option<NetworkError>>,
    // OWNER: 无可用TX reservation时的即时reply丢弃计数；RX仍必须继续，不能阻塞ACK。
    dropped_transmits: Cell<u64>,
    // OWNER: 此PHY实际callback错误与oversize已收帧丢弃；Native proc读取实际设备counter。
    // 外层NET guard独占，无新锁；不能让无法归属flow的硬件故障冒充任意socket SO_ERROR。
    received_errors: Cell<u64>,
    received_dropped: Cell<u64>,
    transmitted_errors: Cell<u64>,
    // OWNER: 主机栈持同一 NET guard，逐批轮转起点，防止繁忙 queue 0 饿死后续队列。
    // Cell 只让存续的批借用与独立 queue cursor 共存，不增加共享并发 owner 或第二把锁。
    receive_queue: Cell<u16>,
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
    pub(super) fn new(
        device: Arc<dyn NetworkDevice>,
        net_id: u64,
        packets: &Arc<packet::PacketRegistry>,
        interface_index: u32,
    ) -> Self {
        Self::with_link(
            EthernetLink::Physical(device),
            net_id,
            packets,
            interface_index,
        )
    }

    pub(super) fn from_vlan(
        link: VlanLink,
        net_id: u64,
        packets: &Arc<packet::PacketRegistry>,
        index: u32,
    ) -> Self {
        Self::with_link(EthernetLink::Vlan(link), net_id, packets, index)
    }

    fn with_link(
        link: EthernetLink,
        net_id: u64,
        packets: &Arc<packet::PacketRegistry>,
        interface_index: u32,
    ) -> Self {
        Self {
            queues: Default::default(),
            link,
            net_id,
            packets: packets.clone(),
            interface_index,
            mtu: ETHERNET_MTU - 14,
            pending_error: Cell::new(None),
            dropped_transmits: Cell::new(0),
            received_errors: Cell::new(0),
            received_dropped: Cell::new(0),
            transmitted_errors: Cell::new(0),
            receive_queue: Cell::new(0),
        }
    }

    /// 随接口安装唯一队列登记，移除或安装失败时自动退休。
    pub(super) fn bind_queues(&mut self, queues: crate::worker::DeviceQueues) {
        self.queues = queues;
    }

    fn vlan(&self) -> Option<&VlanLink> {
        match &self.link {
            EthernetLink::Vlan(link) => Some(link),
            EthernetLink::Physical(_) => None,
        }
    }

    fn nic(&self) -> Option<&dyn NetworkDevice> {
        match &self.link {
            EthernetLink::Physical(device) => Some(device.as_ref()),
            EthernetLink::Vlan(_) => None,
        }
    }

    pub(super) fn network_device(&self) -> Option<Arc<dyn NetworkDevice>> {
        match &self.link {
            EthernetLink::Physical(device) => Some(device.clone()),
            EthernetLink::Vlan(_) => None,
        }
    }

    pub(super) fn mac_address(&self) -> [u8; 6] {
        match &self.link {
            EthernetLink::Physical(device) => device.mac_address(),
            EthernetLink::Vlan(link) => link.mac,
        }
    }

    pub(super) fn transmit_available(&self) -> bool {
        self.nic()
            .is_some_and(|device| device.transmit_available(0))
    }

    pub(super) fn local_statistics(&self) -> NetworkStatistics {
        let mut statistics = NetworkStatistics {
            transmitted_dropped: self.dropped_transmits.get(),
            received_errors: self.received_errors.get(),
            received_dropped: self.received_dropped.get(),
            transmitted_errors: self.transmitted_errors.get(),
            ..Default::default()
        };
        if let Some(link) = self.vlan() {
            statistics.accumulate(link.statistics());
        }
        statistics
    }

    /// 消费网卡进度，记录收发故障并返回发送是否变为可写。
    fn take_transmit_ready(&self) -> bool {
        let Some(device) = self.nic() else {
            return false;
        };
        let progress = device.take_progress();
        if let Some(error) = progress.receive_error {
            self.record_receive_error(error);
        }
        if let Some(error) = progress.transmit_error {
            self.record_transmit_error(error);
        }
        progress.transmit_ready
    }

    /// 消费本轮待上报的网卡故障。
    ///
    /// # Returns
    ///
    /// 优先返回断开，否则返回首个故障。没有故障时返回 `None`。
    pub(super) fn take_error(&self) -> Option<NetworkError> {
        self.pending_error.replace(None)
    }

    fn record_error(&self, error: NetworkError) {
        if error == NetworkError::WouldBlock {
            return;
        }
        if error == NetworkError::Disconnected || self.pending_error.get().is_none() {
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

/// 已完成接收并由本轮收包逻辑消费的帧。
enum EthernetReceive<'a> {
    Physical(NetworkReceiveFrame<'a>),
    Vlan {
        queue: RefMut<'a, vlan::Frames>,
        ip: Option<IngressIp>,
    },
}

impl Deref for EthernetReceive<'_> {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        match self {
            Self::Physical(frame) => frame,
            Self::Vlan { queue, .. } => queue.front().expect("claimed VLAN RX"),
        }
    }
}

impl EthernetReceive<'_> {
    fn ingress_ip(&self) -> Option<IngressIp> {
        match self {
            Self::Physical(frame) => frame.ingress_ip(),
            Self::Vlan { ip, .. } => *ip,
        }
    }

    fn checksum_valid(&self) -> bool {
        matches!(self, Self::Physical(frame) if frame.checksum_valid())
    }

    fn release(self) -> Result<(), NetworkError> {
        match self {
            Self::Physical(frame) => frame.release(),
            Self::Vlan { mut queue, .. } => {
                queue.retire();
                Ok(())
            }
        }
    }
}

pub(super) struct EthernetRxToken<'a> {
    packets: &'a packet::PacketRegistry,
    interface_index: u32,
    mac: [u8; 6],
    frame: EthernetReceive<'a>,
    received_errors: &'a Cell<u64>,
    pending_error: &'a Cell<Option<NetworkError>>,
}

impl EthernetRxToken<'_> {
    fn consume<R>(self, operation: impl FnOnce(&[u8]) -> R) -> R {
        self.consume_frame(|frame| operation(frame))
    }

    fn consume_frame<R>(self, operation: impl FnOnce(&EthernetReceive<'_>) -> R) -> R {
        let frame = &self.frame;
        packet::deliver(
            self.packets,
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
enum EthernetReservation<'a> {
    Physical(NetworkTransmit<'a>),
    Vlan(RefMut<'a, vlan::Frames>),
    // RX 只携带可回复的队列，直到协议确实回复才取得实际发送槽。
    Reply(&'a EthernetDevice, u16),
}

pub(super) struct EthernetTxToken<'a> {
    net_id: u64,
    queues: &'a crate::worker::DeviceQueues,
    packets: &'a packet::PacketRegistry,
    interface_index: u32,
    reservation: Option<EthernetReservation<'a>>,
    // FLAG: 本次 emitter 已明确省略 transport checksum；缺失会把未完成的包误当普通帧发送。
    checksum: bool,
    maximum_frame_length: usize,
    transmitted_errors: &'a Cell<u64>,
    pending_error: &'a Cell<Option<NetworkError>>,
}

impl EthernetTxToken<'_> {
    /// 编码完成后提交实际帧。编码或硬件提交失败返回给调用方，硬件错误进入设备诊断。
    fn consume_committed(
        mut self,
        length: usize,
        operation: impl FnOnce(&mut [u8]) -> bool,
    ) -> Result<(), NetworkError> {
        let result = self.commit_frame(length, operation);
        self.record_transmit_error(result.as_ref().err().copied());
        result
    }

    fn commit_frame(
        &mut self,
        length: usize,
        operation: impl FnOnce(&mut [u8]) -> bool,
    ) -> Result<(), NetworkError> {
        if length > self.maximum_frame_length {
            return Err(NetworkError::FrameTooLarge);
        }
        let reservation = match self.reservation.take() {
            Some(EthernetReservation::Reply(device, queue)) => device.reserve_frame(|_| queue)?,
            Some(reservation) => reservation,
            None => return Err(NetworkError::WouldBlock),
        };
        let render = |frame: &mut [u8]| {
            if !operation(frame) {
                return Err(NetworkError::InvalidConfiguration);
            }
            let checksum = if self.checksum {
                checksum::prepare(frame)?
            } else {
                None
            };
            if length >= 14 {
                packet::deliver_outgoing(
                    self.packets,
                    self.interface_index,
                    1,
                    frame[..14].try_into().unwrap(),
                    &frame[14..],
                    checksum.is_some(),
                    None,
                );
            }
            Ok(checksum)
        };
        match reservation {
            EthernetReservation::Physical(mut reservation) => {
                if length > reservation.capacity() {
                    return Err(NetworkError::FrameTooLarge);
                }
                // emitter 独占最终窗口；未提交时由 reservation 取消槽。
                let checksum = render(&mut reservation.buffer_mut()[..length])?;
                reservation.submit(length, checksum, None)
            }
            EthernetReservation::Vlan(mut queue) => {
                if self.checksum {
                    // VLAN 本身不宣告硬件卸载，拒绝未完成的报文。
                    return Err(NetworkError::InvalidConfiguration);
                }
                let wake = queue.front().is_none();
                let result = queue.write(length, |frame| {
                    let status = render(frame).map(|_| ());
                    (status, status.is_ok())
                });
                if result.is_ok() && wake {
                    crate::worker::request_poll(self.net_id);
                }
                result
            }
            EthernetReservation::Reply(..) => unreachable!("resolved TX reservation"),
        }
    }

    fn record_transmit_error(&self, error: Option<NetworkError>) {
        if let Some(error) = error.filter(|&error| error != NetworkError::WouldBlock) {
            self.transmitted_errors
                .set(self.transmitted_errors.get().saturating_add(1));
            if self.pending_error.get().is_none() {
                self.pending_error.set(Some(error));
            }
        }
    }
}

impl EthernetDevice {
    pub(in crate::inet) fn record_transmit_drop(&self) {
        self.dropped_transmits
            .set(self.dropped_transmits.get().saturating_add(1));
    }
    pub(in crate::inet) fn carrier(&self) -> bool {
        self.link().is_ok_and(|link| link.carrier)
    }
    /// Read the driver's published carrier and epoch without a control RPC.
    pub(in crate::inet) fn link(&self) -> Result<device_model::network::NetworkLink, NetworkError> {
        match &self.link {
            EthernetLink::Physical(device) => device.link(),
            EthernetLink::Vlan(link) => Ok(link.link()),
        }
    }
    /// 报告真实提交结果并记录硬件错误；保留重试不等于丢包，drop 由协议退休/拒绝路径回报。
    pub(in crate::inet) fn send_control(
        &self,
        packet: super::ipv6::Packet<'_>,
    ) -> bool {
        if packet.length() > self.mtu + 14 {
            self.record_transmit_error(NetworkError::FrameTooLarge);
            return false;
        }
        let Ok(token) = self.reserve(|_| 0) else {
            return false;
        };
        token
            .consume_committed(packet.length(), |frame| {
                packet.emit(frame, self.mac_address()).is_ok()
            })
            .is_ok()
    }
}
