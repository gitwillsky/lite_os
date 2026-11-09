//! RFC 9915 DHCPv6 客户端/服务端与中继信封；不拥有租约、前缀分配或重传状态。
pub mod options;
use crate::PacketError;
use core::net::Ipv6Addr;

/// DHCPv6 客户端 UDP 端口。
pub const CLIENT_PORT: u16 = 546;
/// DHCPv6 服务端与中继 UDP 端口。
pub const SERVER_PORT: u16 = 547;
/// 标准转发策略的 hop-count 上限；格式解析保留原值，由 relay 协议决定是否丢弃。
pub const HOP_COUNT_LIMIT: u8 = 8;

/// 基础 DHCPv6 消息类型；接收信封保留未知 Type，不将其映射为已知操作。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum MessageType {
    Solicit = 1,
    Advertise = 2,
    Request = 3,
    Confirm = 4,
    Renew = 5,
    Rebind = 6,
    Reply = 7,
    Release = 8,
    Decline = 9,
    Reconfigure = 10,
    InformationRequest = 11,
    RelayForward = 12,
    RelayReply = 13,
}
impl TryFrom<u8> for MessageType {
    type Error = PacketError;
    fn try_from(value: u8) -> Result<Self, Self::Error> {
        Ok(match value {
            1 => Self::Solicit,
            2 => Self::Advertise,
            3 => Self::Request,
            4 => Self::Confirm,
            5 => Self::Renew,
            6 => Self::Rebind,
            7 => Self::Reply,
            8 => Self::Release,
            9 => Self::Decline,
            10 => Self::Reconfigure,
            11 => Self::InformationRequest,
            12 => Self::RelayForward,
            13 => Self::RelayReply,
            _ => return Err(PacketError::Unsupported),
        })
    }
}

/// 固定头及本层 TLV framing 已校验的借用；嵌套内容按层解析，不递归扫描整个选项树。
#[derive(Clone, Copy)]
pub struct Dhcpv6Packet<'a> {
    bytes: &'a [u8],
    header_length: usize,
}
impl<'a> Dhcpv6Packet<'a> {
    /// 校验 UDP payload 中的固定头和本层所有 TLV 边界；未知类型沿普通信封保留。
    /// # Errors
    /// 固定头或选项声明长度超出输入；不验证 message/option 适用性、必需选项或身份。
    pub fn parse(bytes: &'a [u8]) -> Result<Self, PacketError> {
        let message_type = *bytes.first().ok_or(PacketError::Truncated)?;
        let header_length = if matches!(message_type, 12 | 13) {
            34
        } else {
            4
        };
        options::Options::parse(bytes.get(header_length..).ok_or(PacketError::Truncated)?)?;
        Ok(Self {
            bytes,
            header_length,
        })
    }
    /// 精确的本层消息字节，不包含 UDP 头或其后的其他数据。
    pub fn bytes(self) -> &'a [u8] {
        self.bytes
    }
    /// 原始消息 Type；未知值由对应角色的协议处理者忽略或转发。
    pub fn message_type(self) -> u8 {
        self.bytes[0]
    }
    /// 已校验固定字段范围的信封；普通消息的事务号保留完整 24 位。
    pub fn header(self) -> Dhcpv6Header {
        if self.header_length == 4 {
            Dhcpv6Header::Client {
                message_type: self.bytes[0],
                transaction_id: u32::from_be_bytes([
                    0,
                    self.bytes[1],
                    self.bytes[2],
                    self.bytes[3],
                ]),
            }
        } else {
            Dhcpv6Header::Relay {
                reply: self.bytes[0] == 13,
                hop_count: self.bytes[1],
                link_address: Ipv6Addr::from(
                    <[u8; 16]>::try_from(&self.bytes[2..18]).expect("validated address"),
                ),
                peer_address: Ipv6Addr::from(
                    <[u8; 16]>::try_from(&self.bytes[18..34]).expect("validated address"),
                ),
            }
        }
    }
    /// 按原顺序迭代本层选项；重复项独立存在，不能沿用 DHCPv4 的拼接语义。
    pub fn options(self) -> options::Options<'a> {
        options::Options::validated(&self.bytes[self.header_length..])
    }
    /// 取得本层唯一的 code 选项，供需要 singleton 的协议路径使用。
    /// # Errors
    /// 同一层有多个该 code；不自动把重复数据合并，也不替所有选项决定重复策略。
    pub fn single_option(
        self,
        code: u16,
    ) -> Result<Option<options::Dhcpv6Option<'a>>, PacketError> {
        self.options().single(code)
    }
    /// 从 Relay Message 逐层向内取得普通消息，不使用递归或堆分配。
    /// # Errors
    /// 某层没有唯一 Relay Message，或其中的消息/选项 framing 截断。
    /// 该方法不证明中继方向、hop count、角色、来源或认证有效，不能绕过 relay 协议验证。
    pub fn innermost(mut self) -> Result<Self, PacketError> {
        // 1. 每次仅解析本层 TLV；2. 子消息必须来自该层的 option 数据；3. 长度严格缩小保证终止。
        while self.header_length == 34 {
            let inner = self.single_option(9)?.ok_or(PacketError::Malformed)?;
            self = Self::parse(inner.data())?;
        }
        Ok(self)
    }
}

/// 普通四字节信封与中继 34 字节信封的固定字段，完整选项由调用者先在对应偏移就位。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dhcpv6Header {
    /// 客户端/服务端普通消息；message_type 不能伪装为 RelayForward/RelayReply。
    Client {
        message_type: u8,
        transaction_id: u32,
    },
    /// 中继字段；hop-count 准入与 link/peer 的语义归中继协议。
    Relay {
        reply: bool,
        hop_count: u8,
        link_address: Ipv6Addr,
        peer_address: Ipv6Addr,
    },
}
impl Dhcpv6Header {
    /// 固定信封字节数，不包含选项。
    pub fn header_length(self) -> usize {
        match self {
            Self::Client { .. } => 4,
            Self::Relay { .. } => 34,
        }
    }
    /// 原位填写固定头，不复制已就位的选项，不要求理解未知选项。
    /// # Errors
    /// 固定头空间不足、事务号超过 24 位、普通信封使用中继 Type 或选项 framing 损坏；失败不写 packet。
    pub fn emit(self, packet: &mut [u8]) -> Result<(), PacketError> {
        // 1. 输出范围和所有可失败检查先于写入；不读取尚未初始化的旧固定头。
        let header_length = self.header_length();
        let options = packet.get(header_length..).ok_or(PacketError::Capacity)?;
        if let Self::Client {
            message_type,
            transaction_id,
        } = self
            && (matches!(message_type, 12 | 13) || transaction_id > 0xff_ffff)
        {
            return Err(PacketError::Malformed);
        }
        options::Options::parse(options)?;
        // 2. DHCPv6 不自带 checksum，由 UDP/IPv6 的实际伪头覆盖整个 payload。
        match self {
            Self::Client {
                message_type,
                transaction_id,
            } => {
                packet[0] = message_type;
                packet[1..4].copy_from_slice(&transaction_id.to_be_bytes()[1..]);
            }
            Self::Relay {
                reply,
                hop_count,
                link_address,
                peer_address,
            } => {
                packet[0] = if reply { 13 } else { 12 };
                packet[1] = hop_count;
                packet[2..18].copy_from_slice(&link_address.octets());
                packet[18..34].copy_from_slice(&peer_address.octets());
            }
        }
        Ok(())
    }
}
