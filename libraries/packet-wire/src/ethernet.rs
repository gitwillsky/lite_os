//! Ethernet MAC header、802.1Q C/S tags 与 802.3 Length/Type；不包含 preamble 或 FCS。
use crate::{PacketError, buffer::PacketBuffer};

/// 基础 MAC header 长度，不含 VLAN tags。
pub const HEADER_LENGTH: usize = 14;
/// IEEE 802.1Q customer tag EtherType。
pub const CUSTOMER_TAG: u16 = 0x8100;
/// IEEE 802.1Q service tag EtherType。
pub const SERVICE_TAG: u16 = 0x88a8;

/// 原始 Ethernet frame 借用；只验证基础 header，供抓包完整保留标签、未知协议与 padding。
#[derive(Clone, Copy)]
pub struct EthernetFrame<'a> {
    bytes: &'a [u8],
}
impl<'a> EthernetFrame<'a> {
    /// 验证 `bytes` 至少包含基础 MAC header；不替抓包者丢弃格式未知的 payload。
    /// # Errors
    /// 长度不足 14 字节时返回 Truncated。
    #[inline]
    pub fn parse(bytes: &'a [u8]) -> Result<Self, PacketError> {
        if bytes.len() < HEADER_LENGTH {
            return Err(PacketError::Truncated);
        }
        Ok(Self { bytes })
    }
    /// 基础 MAC header 原字节，标签 TCI 仍属于其后数据。
    #[inline]
    pub fn header(self) -> &'a [u8; HEADER_LENGTH] {
        self.bytes[..HEADER_LENGTH]
            .try_into()
            .expect("validated MAC header")
    }
    /// 外层 Length/Type 字段，不自动把 VLAN 内层协议暴露给原始抓包。
    #[inline]
    pub fn type_or_length(self) -> u16 {
        u16::from_be_bytes([self.bytes[12], self.bytes[13]])
    }
    /// 源 MAC 地址。
    #[inline]
    pub fn source(self) -> [u8; 6] {
        self.bytes[6..12].try_into().expect("validated MAC header")
    }
    /// 目的 MAC 地址。
    #[inline]
    pub fn destination(self) -> [u8; 6] {
        self.bytes[..6].try_into().expect("validated MAC header")
    }
    /// 基础 MAC header 之后的全部原字节，保留标签与 padding。
    #[inline]
    pub fn payload(self) -> &'a [u8] {
        &self.bytes[HEADER_LENGTH..]
    }
    /// 逐层解开标准 C/S tags 后提供网络 payload；802.3 length 帧按声明长度去掉 padding。
    /// # Errors
    /// tag 截断、长度字段超出输入，或内层 Length/Type 落入 1501..1535 的保留区间。
    pub fn network(self) -> Result<EthernetPayload<'a>, PacketError> {
        let mut protocol = self.type_or_length();
        let mut offset = HEADER_LENGTH;
        while is_tag(protocol) {
            let tag = self
                .bytes
                .get(offset..offset + 4)
                .ok_or(PacketError::Truncated)?;
            protocol = u16::from_be_bytes([tag[2], tag[3]]);
            offset += 4;
        }
        let mut payload = &self.bytes[offset..];
        if protocol <= 1500 {
            payload = payload
                .get(..usize::from(protocol))
                .ok_or(PacketError::Truncated)?;
        } else if protocol < 1536 {
            return Err(PacketError::Malformed);
        }
        Ok(EthernetPayload {
            type_or_length: protocol,
            tags: &self.bytes[HEADER_LENGTH..offset],
            outer: self.type_or_length(),
            payload,
        })
    }
}

/// 一层标准 VLAN tag 的 wire 字段；VID 0/4095 等的接收策略由 VLAN/bridge owner 判定。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VlanTag {
    pub protocol: u16,
    pub control: u16,
}
impl VlanTag {
    /// 12-bit VLAN ID；0 是 priority tag，4095 保留，不在 codec 中改写为其他 VID。
    pub fn id(self) -> u16 {
        self.control & 0x0fff
    }
    /// 三位 Priority Code Point。
    pub fn priority(self) -> u8 {
        (self.control >> 13) as u8
    }
    /// Drop Eligible Indicator。
    pub fn drop_eligible(self) -> bool {
        self.control & 0x1000 != 0
    }
}

/// 已解开的网络层视图；基础抓包 frame 与该视图分别保留外/内层语义。
pub struct EthernetPayload<'a> {
    type_or_length: u16,
    tags: &'a [u8],
    outer: u16,
    payload: &'a [u8],
}
impl<'a> EthernetPayload<'a> {
    /// 最内层 Length/Type；小于等于 1500 时 payload 是 LLC 数据，不是 EtherType 协议包。
    pub fn type_or_length(&self) -> u16 {
        self.type_or_length
    }
    /// 去掉 VLAN tags 和可识别的 802.3 padding 后的借用。
    pub fn payload(&self) -> &'a [u8] {
        self.payload
    }
    /// 从最外层到最内层遍历已验证的 VLAN tags，无 Vec 或固定 tag 层数假设。
    pub fn tags(&self) -> impl ExactSizeIterator<Item = VlanTag> + '_ {
        let mut protocol = self.outer;
        self.tags.as_chunks::<4>().0.iter().map(move |bytes| {
            let result = VlanTag {
                protocol,
                control: u16::from_be_bytes([bytes[0], bytes[1]]),
            };
            protocol = u16::from_be_bytes([bytes[2], bytes[3]]);
            result
        })
    }
}

/// 输出 MAC header 的字段与从外向内的 tag 序列；payload 保持在现有位置。
pub struct EthernetHeader<'a> {
    pub destination: [u8; 6],
    pub source: [u8; 6],
    /// 最内层 Length/Type；不限制 raw packet socket 可发送的协议字段。
    pub type_or_length: u16,
    pub tags: &'a [VlanTag],
}
impl EthernetHeader<'_> {
    /// 含全部 tags 的 header 大小。
    pub fn length(&self) -> usize {
        HEADER_LENGTH + self.tags.len() * 4
    }
    /// 在 `output` 的前部写完整 header，返回写入长度；不读写后面的 payload。
    /// # Errors
    /// tag 协议不是标准 C/S TPID，或输出不足；失败不改变输出。
    pub fn emit(&self, output: &mut [u8]) -> Result<usize, PacketError> {
        self.validate()?;
        let length = self.length();
        let output = output.get_mut(..length).ok_or(PacketError::Capacity)?;
        self.write(output);
        Ok(length)
    }
    /// 在预留 headroom 中写 header；不移动原 payload，失败不改变 packet window。
    /// # Errors
    /// 非标准 tag TPID 或前部空间不足。
    pub fn prepend(&self, packet: &mut PacketBuffer<'_>) -> Result<(), PacketError> {
        self.validate()?;
        let output = packet.prepend(self.length())?;
        self.write(output);
        Ok(())
    }
    fn validate(&self) -> Result<(), PacketError> {
        if self.tags.iter().any(|tag| !is_tag(tag.protocol)) {
            return Err(PacketError::Malformed);
        }
        Ok(())
    }
    fn write(&self, output: &mut [u8]) {
        output[..6].copy_from_slice(&self.destination);
        output[6..12].copy_from_slice(&self.source);
        let mut offset = 12;
        for tag in self.tags {
            output[offset..offset + 2].copy_from_slice(&tag.protocol.to_be_bytes());
            output[offset + 2..offset + 4].copy_from_slice(&tag.control.to_be_bytes());
            offset += 4;
        }
        output[offset..offset + 2].copy_from_slice(&self.type_or_length.to_be_bytes());
    }
}
fn is_tag(protocol: u16) -> bool {
    matches!(protocol, CUSTOMER_TAG | SERVICE_TAG)
}
