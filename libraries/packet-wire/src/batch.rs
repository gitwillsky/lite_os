//! 分阶段处理一批独立 Ethernet 借用：先完成所有链路边界检查，再解析网络头。
//! 不拥有帧、队列或设备；不校验 transport，不把格式事实当作协议接收决定。

use crate::{
    PacketError,
    arp::ArpPacket,
    ethernet::EthernetFrame,
    ipv4::Ipv4Packet,
    ipv6::{Ipv6Datagram, Ipv6Error},
};

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
