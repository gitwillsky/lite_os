//! 分阶段处理一批独立 Ethernet 借用：先完成所有链路边界检查，再解析网络头。
//! 不拥有帧、队列或设备；格式和校验事实不代表协议接收决定。

use crate::{
    PacketError,
    arp::ArpPacket,
    ethernet::EthernetFrame,
    ipv4::Ipv4Packet,
    ipv6::{Ipv6Datagram, Ipv6Error, is_parsed_extension},
};
use core::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// 从一帧获得的 IP 结构事实与解析边界。不持有原帧，也不表示通过协议接收检查。
#[derive(Clone, Copy)]
pub struct IngressIp(IpHeader);

#[derive(Clone, Copy)]
enum IpHeader {
    Ipv4 {
        source: Ipv4Addr,
        destination: Ipv4Addr,
        protocol: u8,
        fragmented: bool,
        length: [u8; 2],
        header: u8,
    },
    Ipv6 {
        destination: Ipv6Addr,
        starts_extension_chain: bool,
        // 字节数组避免为每帧的结构事实增加对齐填充。
        payload: [u8; 4],
    },
}

impl IngressIp {
    /// 从已检查结构的 IPv4 头提取可脱离原帧保存的事实。
    pub fn from_ipv4(packet: Ipv4Packet<'_>) -> Self {
        Self(IpHeader::Ipv4 {
            source: packet.source(),
            destination: packet.destination(),
            protocol: packet.protocol(),
            fragmented: packet.fragmented(),
            length: (packet.bytes().len() as u16).to_ne_bytes(),
            header: (packet.bytes().len() - packet.payload().len()) as u8,
        })
    }

    /// 检查以太网的直接 IP 载荷。VLAN、ARP、其他 EtherType 或畸形帧返回 None。
    /// 标签准入属于网桥，头校验和、扩展链和传输语义仍由相应协议检查。
    pub fn from_ethernet(bytes: &[u8]) -> Option<Self> {
        NetworkPacket::direct_ip(bytes)?.ingress_ip()
    }

    /// 在原始帧上借出已检查的直接 IP 载荷，保留解析时排除的链路 padding。
    /// # Safety
    /// `frame` 必须是产生结构事实的原始 Ethernet 帧，且其字节从解析后到本次借用结束始终不变。
    /// 此方法不验证 IPv4 头校验和。
    pub unsafe fn borrow_direct<'a>(&self, frame: &'a [u8]) -> NetworkPacket<'a> {
        use crate::ethernet::HEADER_LENGTH;
        let payload = &frame[HEADER_LENGTH..];
        match self.0 {
            IpHeader::Ipv4 { length, header, .. } => NetworkPacket::Ipv4(
                Ipv4Packet::from_checked_bounds(
                    &payload[..usize::from(u16::from_ne_bytes(length))],
                    usize::from(header),
                ),
            ),
            IpHeader::Ipv6 { payload: length, .. } => NetworkPacket::Ipv6(
                Ipv6Datagram::from_checked_bounds(
                    &payload[..40 + u32::from_ne_bytes(length) as usize],
                ),
            ),
        }
    }

    /// 返回 IPv4 源和目的，IPv6 返回 None。
    pub fn addresses(self) -> Option<(Ipv4Addr, Ipv4Addr)> {
        match self.0 {
            IpHeader::Ipv4 {
                source,
                destination,
                ..
            } => Some((source, destination)),
            IpHeader::Ipv6 { .. } => None,
        }
    }

    /// 返回固定头的目的地址。
    pub fn destination(self) -> IpAddr {
        match self.0 {
            IpHeader::Ipv4 { destination, .. } => destination.into(),
            IpHeader::Ipv6 { destination, .. } => destination.into(),
        }
    }

    /// 此结构事实来自 IPv6 固定头。
    pub fn is_ipv6(self) -> bool {
        matches!(self.0, IpHeader::Ipv6 { .. })
    }

    /// 本机重组入口是否仍需检查此帧。IPv6 扩展链也可能需要报告 framing 错误。
    pub fn may_need_reassembly(self) -> bool {
        match self.0 {
            IpHeader::Ipv4 { fragmented, .. } => fragmented,
            IpHeader::Ipv6 { starts_extension_chain, .. } => starts_extension_chain,
        }
    }

    /// 返回原 IPv4 Protocol 字段。IPv6 返回 None。
    pub fn ipv4_protocol(self) -> Option<u8> {
        match self.0 {
            IpHeader::Ipv4 { protocol, .. } => Some(protocol),
            IpHeader::Ipv6 { .. } => None,
        }
    }
}

/// 一帧的格式错误；IPv6 保留固定头和长度发现错误的精确位置。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameError {
    /// Ethernet、ARP 或 IPv4 格式错误。
    Packet(PacketError),
    /// IPv6 固定头、数据报长度或必要 Jumbo 长度发现错误。
    Ipv6(Ipv6Error),
}

/// 已检查边界的网络层；分片仍属于重组 owner，不解释其 transport。
#[derive(Clone, Copy)]
pub enum NetworkPacket<'a> {
    /// 头部、总长度、选项与分片结构已检查，头 checksum 仍须由协议引擎验证。
    Ipv4(Ipv4Packet<'a>),
    /// 固定头/数据报长度已检查；逐跳选项与终点扩展链由各自协议入口处理。
    Ipv6(Ipv6Datagram<'a>),
    /// 通用 ARP 结构；地址类型和操作码由邻居 owner 判断。
    Arp(ArpPacket<'a>),
    /// 未知 EtherType 或 802.3 LLC，保留精确内层协议与裁过 padding 的原字节。
    Other { protocol: u16, bytes: &'a [u8] },
}

impl<'a> NetworkPacket<'a> {
    /// 借用以太网直接承载的 IP 包。VLAN、非 IP 或无效边界不返回借用。
    pub fn direct_ip(bytes: &'a [u8]) -> Option<Self> {
        let ethernet = EthernetFrame::parse(bytes).ok()?;
        match ethernet.type_or_length() {
            0x0800 => Ipv4Packet::parse_header(ethernet.payload())
                .ok()
                .map(Self::Ipv4),
            0x86dd => Ipv6Datagram::parse(ethernet.payload()).ok().map(Self::Ipv6),
            _ => None,
        }
    }

    /// 捕获脱离原帧借用仍有效的 IP 头部事实。
    pub fn ingress_ip(self) -> Option<IngressIp> {
        match self {
            Self::Ipv4(packet) => Some(IngressIp::from_ipv4(packet)),
            Self::Ipv6(packet) => Some(IngressIp(IpHeader::Ipv6 {
                destination: packet.destination(),
                starts_extension_chain: is_parsed_extension(packet.bytes()[6]),
                payload: (packet.payload().len() as u32).to_ne_bytes(),
            })),
            _ => None,
        }
    }

    /// 校验可直接定位的 TCP、UDP 或 ICMP。分片、IPv6 扩展头及其他协议返回 None，
    /// 由消费方处理。UDP 按线上的长度字段验证，保留两族不同的零 checksum 语义。
    /// 结果仅证明本报文的 checksum，TCP 与 ICMP 的结构仍须由消费方检查。
    pub fn verify_direct_transport(self) -> Option<bool> {
        let (source, destination, protocol, payload) = self.direct_transport()?;
        match (source, protocol) {
            (IpAddr::V4(_), 1) => Some(crate::checksum::checksum(payload) == 0),
            (_, 17) => Some(crate::udp::UdpPacket::parse(payload, source, destination).is_ok()),
            (_, 6) | (IpAddr::V6(_), 58) => Some(matches!(
                crate::checksum::transport_checksum(source, destination, protocol, payload),
                Ok(0)
            )),
            _ => None,
        }
    }

    /// 返回不需重组或处理扩展头即可定位的传输层范围。调用者仍须验证其格式和校验和。
    pub fn direct_transport(self) -> Option<(IpAddr, IpAddr, u8, &'a [u8])> {
        match self {
            Self::Ipv4(packet) if !packet.fragmented() => Some((
                IpAddr::V4(packet.source()),
                IpAddr::V4(packet.destination()),
                packet.protocol(),
                packet.payload(),
            )),
            Self::Ipv6(packet) if matches!(packet.bytes()[6], 6 | 17 | 58) => Some((
                IpAddr::V6(packet.source()),
                IpAddr::V6(packet.destination()),
                packet.bytes()[6],
                &packet.bytes()[40..],
            )),
            _ => None,
        }
    }
}

/// 链路与网络头共用原帧借用；原始帧仍包含 VLAN/padding，不能用网络视图替换抓包内容。
#[derive(Clone, Copy)]
pub struct ParsedFrame<'a> {
    /// 原始 MAC 头与完整帧。
    pub ethernet: EthernetFrame<'a>,
    /// 去掉标准 VLAN 头/可识别链路 padding 后的网络头。
    pub network: NetworkPacket<'a>,
}

/// 在调用者提供的槽位中解析整批帧，不分配或复制 payload。
///
/// # Parameters
/// `input` 每项是一帧的不可变借用，`output` 长度必须相同；批大小由调用者的预算决定。
/// # Returns
/// 每个输出槽独立记录格式结果，一帧失败不丢弃同批其他帧，不重排输入顺序。
/// # Errors
/// 槽位数量不同返回 Capacity，输出保持原状；单帧错误留在对应输出槽。
pub fn parse_ethernet<'a>(
    input: impl ExactSizeIterator<Item = &'a [u8]>,
    output: &mut [Result<ParsedFrame<'a>, FrameError>],
) -> Result<(), PacketError> {
    if input.len() != output.len() {
        return Err(PacketError::Capacity);
    }
    // 1. 先推进整个批次的 MAC/VLAN/Length-Type 边界；没有跨帧推测读取或固定 tag 层数。
    for (bytes, slot) in input.zip(output.iter_mut()) {
        *slot = (|| {
            let ethernet = EthernetFrame::parse(bytes)?;
            let network = ethernet.network()?;
            Ok(ParsedFrame {
                ethernet,
                network: NetworkPacket::Other {
                    protocol: network.type_or_length(),
                    bytes: network.payload(),
                },
            })
        })()
        .map_err(FrameError::Packet);
    }
    // 2. 原位把同一组已检查边界转换为类型化头；不重复解 VLAN，不验证/消费 transport 状态。
    for slot in output {
        let Ok(frame) = slot else { continue };
        let NetworkPacket::Other { protocol, bytes } = frame.network else {
            unreachable!("first phase only publishes bounded link payloads")
        };
        let network = match protocol {
            0x0800 => Ipv4Packet::parse_header(bytes)
                .map(NetworkPacket::Ipv4)
                .map_err(FrameError::Packet),
            0x86dd => Ipv6Datagram::parse(bytes)
                .map(NetworkPacket::Ipv6)
                .map_err(FrameError::Ipv6),
            0x0806 => ArpPacket::parse(bytes)
                .map(NetworkPacket::Arp)
                .map_err(FrameError::Packet),
            _ => Ok(NetworkPacket::Other { protocol, bytes }),
        };
        match network {
            Ok(network) => frame.network = network,
            Err(error) => *slot = Err(error),
        }
    }
    Ok(())
}
