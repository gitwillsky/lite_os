//! RFC 4861 邻居发现消息与八字节单位选项；不拥有邻居、路由或地址配置状态。
pub mod options;
use crate::{
    PacketError,
    icmpv6::{self, Icmpv6Packet},
};
use core::net::Ipv6Addr;

/// 固定消息和完整选项边界已校验的 ND 借用；不替代 IPv6 hop limit、地址与安全策略验证。
#[derive(Clone, Copy)]
pub struct NdPacket<'a> {
    packet: Icmpv6Packet<'a>,
    header_length: usize,
}
impl<'a> NdPacket<'a> {
    /// 从 ICMPv6 借用检查 ND 类型、Code、固定字段与选项长度；不会再次扫描 checksum。
    /// # Errors
    /// 非 ND 类型、非零 Code、固定消息截断或选项长度为零/超出消息。
    /// 调用者必须使用已验证 checksum 的 Packet，或持有硬件等价验证事实。
    pub fn parse(packet: Icmpv6Packet<'a>) -> Result<Self, PacketError> {
        let header_length = match packet.message_type() {
            133 => 8,
            134 => 16,
            135 | 136 => 24,
            137 => 40,
            _ => return Err(PacketError::Unsupported),
        };
        if packet.code() != 0 {
            return Err(PacketError::Malformed);
        }
        let bytes = packet.bytes();
        let options = bytes.get(header_length..).ok_or(PacketError::Truncated)?;
        options::Options::parse(options)?;
        Ok(Self {
            packet,
            header_length,
        })
    }
    /// 已校验的固定消息；接收保留位不导致拒包。
    pub fn message(self) -> NdMessage {
        let bytes = self.packet.bytes();
        match self.packet.message_type() {
            133 => NdMessage::RouterSolicitation,
            134 => NdMessage::RouterAdvertisement {
                hop_limit: bytes[4],
                flags: bytes[5],
                lifetime: u16::from_be_bytes([bytes[6], bytes[7]]),
                reachable_time: u32::from_be_bytes(
                    bytes[8..12].try_into().expect("validated field"),
                ),
                retransmit_timer: u32::from_be_bytes(
                    bytes[12..16].try_into().expect("validated field"),
                ),
            },
            135 => NdMessage::NeighborSolicitation {
                target: address(&bytes[8..24]),
            },
            136 => NdMessage::NeighborAdvertisement {
                flags: u32::from_be_bytes(bytes[4..8].try_into().expect("validated field")),
                target: address(&bytes[8..24]),
            },
            137 => NdMessage::Redirect {
                target: address(&bytes[8..24]),
                destination: address(&bytes[24..40]),
            },
            _ => unreachable!("ND Type validated"),
        }
    }
    /// 无分配遍历全部选项；每项 decode 的已知布局错误独立于整个消息的边界验证。
    pub fn options(self) -> options::Options<'a> {
        options::Options::validated(&self.packet.bytes()[self.header_length..])
    }
}

fn address(bytes: &[u8]) -> Ipv6Addr {
    Ipv6Addr::from(<[u8; 16]>::try_from(bytes).expect("validated IPv6 address"))
}

/// ND 的固定字段原值；RA flags 由调用者按已启用的扩展设置，其余保留字段输出为零。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NdMessage {
    /// Router Solicitation，Reserved 输出为零。
    RouterSolicitation,
    /// Router Advertisement；时间的单位按 RFC 分别为秒和毫秒。
    RouterAdvertisement {
        hop_limit: u8,
        flags: u8,
        lifetime: u16,
        reachable_time: u32,
        retransmit_timer: u32,
    },
    /// Neighbor Solicitation；组播 target 与 DAD 检查属于邻居协议。
    NeighborSolicitation { target: Ipv6Addr },
    /// Neighbor Advertisement；R/S/O 位位于 flags 的最高三位。
    NeighborAdvertisement { flags: u32, target: Ipv6Addr },
    /// Redirect；被重定向包位于选项中，不在固定消息内。
    Redirect {
        target: Ipv6Addr,
        destination: Ipv6Addr,
    },
}
impl NdMessage {
    /// 包含 ICMPv6 共同头的固定消息字节数，不包含选项。
    pub fn header_length(self) -> usize {
        match self {
            Self::RouterSolicitation => 8,
            Self::RouterAdvertisement { .. } => 16,
            Self::NeighborSolicitation { .. } | Self::NeighborAdvertisement { .. } => 24,
            Self::Redirect { .. } => 40,
        }
    }
    /// 在完整 packet 中填写固定字段与 checksum，选项先由调用者在 header_length 之后构造。
    /// # Errors
    /// 固定头空间不足、长度不可表达或选项 framing/已知布局非法；失败不写 packet。
    /// hop limit 255、地址/生命周期及选项适用消息的策略仍由协议处理者校验。
    pub fn emit(
        self,
        packet: &mut [u8],
        source: Ipv6Addr,
        destination: Ipv6Addr,
    ) -> Result<(), PacketError> {
        // 1. 在写入前检查全部选项；接收时可忽略的坏选项不允许由本机编码发出。
        let length = self.header_length();
        let bytes = packet.get(length..).ok_or(PacketError::Capacity)?;
        u32::try_from(packet.len()).map_err(|_| PacketError::Malformed)?;
        for option in options::Options::parse(bytes)? {
            option.decode()?;
        }
        // 2. 清零保留字段，保留已就位的选项；不分配或搬移它们。
        packet[..length].fill(0);
        let message_type = match self {
            Self::RouterSolicitation => 133,
            Self::RouterAdvertisement {
                hop_limit,
                flags,
                lifetime,
                reachable_time,
                retransmit_timer,
            } => {
                packet[4] = hop_limit;
                // flags 存在独立标准扩展，不在基础 codec 中用固定掩码悄悄删除调用者的位。
                packet[5] = flags;
                packet[6..8].copy_from_slice(&lifetime.to_be_bytes());
                packet[8..12].copy_from_slice(&reachable_time.to_be_bytes());
                packet[12..16].copy_from_slice(&retransmit_timer.to_be_bytes());
                134
            }
            Self::NeighborSolicitation { target } => {
                packet[8..24].copy_from_slice(&target.octets());
                135
            }
            Self::NeighborAdvertisement { flags, target } => {
                packet[4..8].copy_from_slice(&(flags & 0xe000_0000).to_be_bytes());
                packet[8..24].copy_from_slice(&target.octets());
                136
            }
            Self::Redirect {
                target,
                destination,
            } => {
                packet[8..24].copy_from_slice(&target.octets());
                packet[24..40].copy_from_slice(&destination.octets());
                137
            }
        };
        // 3. 长度和地址族已预检；此处只有一次完整伪头 checksum 扫描。
        icmpv6::emit(packet, message_type, 0, source, destination)
            .expect("prevalidated ND envelope");
        Ok(())
    }
}
