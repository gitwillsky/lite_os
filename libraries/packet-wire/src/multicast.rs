//! RFC 9776/9777 IGMPv3/MLDv2 共有的 source-filter 字节布局；不拥有成员关系、计时器或路由。
use crate::PacketError;
use core::net::{IpAddr, Ipv4Addr, Ipv6Addr};

#[derive(Clone, Copy)]
pub(crate) enum Family {
    V4,
    V6,
}
impl Family {
    fn width(self) -> usize {
        match self {
            Self::V4 => 4,
            Self::V6 => 16,
        }
    }
    fn address(self, bytes: &[u8]) -> IpAddr {
        match self {
            Self::V4 => {
                Ipv4Addr::from(<[u8; 4]>::try_from(bytes).expect("validated address")).into()
            }
            Self::V6 => {
                Ipv6Addr::from(<[u8; 16]>::try_from(bytes).expect("validated address")).into()
            }
        }
    }
}

/// 来源地址的零分配迭代器；每项宽度由所属 IGMP/MLD 消息决定。
#[derive(Clone, Copy)]
pub struct SourceAddresses<'a> {
    bytes: &'a [u8],
    family: Family,
}
impl<'a> SourceAddresses<'a> {
    pub(crate) fn parse(bytes: &'a [u8], count: u16, family: Family) -> Result<Self, PacketError> {
        let length = usize::from(count) * family.width();
        Ok(Self {
            bytes: bytes.get(..length).ok_or(PacketError::Truncated)?,
            family,
        })
    }
    /// 源地址向量的网络字节序原字节，不包含额外数据。
    pub fn bytes(self) -> &'a [u8] {
        self.bytes
    }
}
impl Iterator for SourceAddresses<'_> {
    type Item = IpAddr;
    fn next(&mut self) -> Option<Self::Item> {
        if self.bytes.is_empty() {
            return None;
        }
        let (address, rest) = self.bytes.split_at(self.family.width());
        self.bytes = rest;
        Some(self.family.address(address))
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        let length = self.bytes.len() / self.family.width();
        (length, Some(length))
    }
}
impl ExactSizeIterator for SourceAddresses<'_> {}

/// S/QRV/QQIC 与源列表；保留网络编码，robustness 为零等默认策略由成员协议处理。
#[derive(Clone, Copy)]
pub struct SourceQuery<'a> {
    /// 查询头高四位的扩展 flags；基础成员协议不能因未知位拒包。
    pub flags: u8,
    pub suppress: bool,
    pub robustness: u8,
    pub query_interval_code: u8,
    pub sources: SourceAddresses<'a>,
}
impl<'a> SourceQuery<'a> {
    pub(crate) fn parse(bytes: &'a [u8], family: Family) -> Result<Self, PacketError> {
        if bytes.len() < 4 {
            return Err(PacketError::Truncated);
        }
        Ok(Self {
            flags: bytes[0] >> 4,
            suppress: bytes[0] & 8 != 0,
            robustness: bytes[0] & 7,
            query_interval_code: bytes[1],
            sources: SourceAddresses::parse(
                &bytes[4..],
                u16::from_be_bytes([bytes[2], bytes[3]]),
                family,
            )?,
        })
    }
}

/// 查询的 source-filter 头；源地址已紧跟四字节头就位，不在编码时搬移。
#[derive(Clone, Copy)]
pub struct SourceQueryHeader {
    /// 查询头的四位扩展 flags，未启用扩展时为零。
    pub flags: u8,
    pub suppress: bool,
    pub robustness: u8,
    pub query_interval_code: u8,
    pub source_count: u16,
}
impl SourceQueryHeader {
    pub(crate) fn validate(self, length: usize, family: Family) -> Result<(), PacketError> {
        if self.robustness > 7 || self.flags > 15 {
            return Err(PacketError::Malformed);
        }
        let required = 4 + usize::from(self.source_count) * family.width();
        if length != required {
            return Err(PacketError::Malformed);
        }
        Ok(())
    }
    pub(crate) fn write(self, bytes: &mut [u8]) {
        bytes[0] = (self.flags << 4) | (u8::from(self.suppress) << 3) | self.robustness;
        bytes[1] = self.query_interval_code;
        bytes[2..4].copy_from_slice(&self.source_count.to_be_bytes());
    }
}

/// IGMPv3/MLDv2 的六种标准地址记录类型。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum RecordType {
    ModeIsInclude = 1,
    ModeIsExclude = 2,
    ChangeToInclude = 3,
    ChangeToExclude = 4,
    AllowNewSources = 5,
    BlockOldSources = 6,
}
impl TryFrom<u8> for RecordType {
    type Error = PacketError;
    /// 转换已定义的记录类型，未知值返回 Unsupported；接收迭代器仍保留其原字节。
    fn try_from(value: u8) -> Result<Self, Self::Error> {
        Ok(match value {
            1 => Self::ModeIsInclude,
            2 => Self::ModeIsExclude,
            3 => Self::ChangeToInclude,
            4 => Self::ChangeToExclude,
            5 => Self::AllowNewSources,
            6 => Self::BlockOldSources,
            _ => return Err(PacketError::Unsupported),
        })
    }
}

/// 按报告中的 count 验证的完整记录序列；额外尾部不混入最后一条记录。
#[derive(Clone, Copy)]
pub struct GroupRecords<'a> {
    bytes: &'a [u8],
    remaining: u16,
    family: Family,
}
impl<'a> GroupRecords<'a> {
    pub(crate) fn parse(bytes: &'a [u8], count: u16, family: Family) -> Result<Self, PacketError> {
        let mut rest = bytes;
        for _ in 0..count {
            let length = record_length(rest, family)?;
            rest = rest.get(length..).ok_or(PacketError::Truncated)?;
        }
        Ok(Self {
            bytes: &bytes[..bytes.len() - rest.len()],
            remaining: count,
            family,
        })
    }
    /// 仅包含 count 条记录的字节；查询/报告中的额外接收尾部不在其中。
    pub fn bytes(self) -> &'a [u8] {
        self.bytes
    }
    pub(crate) fn validate_output(self, length: usize) -> Result<(), PacketError> {
        if self.bytes.len() != length {
            return Err(PacketError::Malformed);
        }
        for record in self {
            RecordType::try_from(record.record_type())?;
            if !record.auxiliary_data().is_empty() {
                return Err(PacketError::Malformed);
            }
        }
        Ok(())
    }
}
fn record_length(bytes: &[u8], family: Family) -> Result<usize, PacketError> {
    if bytes.len() < 4 + family.width() {
        return Err(PacketError::Truncated);
    }
    let count = usize::from(u16::from_be_bytes([bytes[2], bytes[3]]));
    // count 最多 65535、地址最多 16 字节、辅助区最多 255 words；总长在 32-bit usize 内也不溢出。
    Ok(4 + family.width() * (1 + count) + usize::from(bytes[1]) * 4)
}
impl<'a> Iterator for GroupRecords<'a> {
    type Item = GroupRecord<'a>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        let length = record_length(self.bytes, self.family).expect("record range validated");
        let (bytes, rest) = self.bytes.split_at(length);
        self.bytes = rest;
        self.remaining -= 1;
        Some(GroupRecord {
            bytes,
            family: self.family,
        })
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        let length = usize::from(self.remaining);
        (length, Some(length))
    }
}
impl ExactSizeIterator for GroupRecords<'_> {}

/// 组地址、源向量与辅助区均已验证范围的单条地址记录；未知 Type 由成员协议忽略。
#[derive(Clone, Copy)]
pub struct GroupRecord<'a> {
    bytes: &'a [u8],
    family: Family,
}
impl<'a> GroupRecord<'a> {
    /// 原始记录类型，不把未知值当作 INCLUDE/EXCLUDE。
    pub fn record_type(self) -> u8 {
        self.bytes[0]
    }
    /// 原始组地址；是否为合法组播地址由协议验证。
    pub fn group(self) -> IpAddr {
        self.family.address(&self.bytes[4..4 + self.family.width()])
    }
    /// 精确 count 的来源列表，不包括辅助区。
    pub fn sources(self) -> SourceAddresses<'a> {
        SourceAddresses::parse(
            &self.bytes[4 + self.family.width()..],
            u16::from_be_bytes([self.bytes[2], self.bytes[3]]),
            self.family,
        )
        .expect("validated sources")
    }
    /// 按 32-bit word 长度取得辅助数据，当前基础成员协议忽略它而非丢弃报告。
    pub fn auxiliary_data(self) -> &'a [u8] {
        &self.bytes[self.bytes.len() - usize::from(self.bytes[1]) * 4..]
    }
}

/// 输出无辅助数据的标准记录头；源字节在 packet 的头之后已经就位。
pub struct GroupRecordHeader {
    pub record_type: RecordType,
    pub group: IpAddr,
    pub source_count: u16,
}
impl GroupRecordHeader {
    /// 构造记录头，返回包含来源列表的记录字节数；不移动来源列表，也不写其后的其他记录。
    /// # Errors
    /// packet 放不下完整记录；失败不写 packet。来源地址族由 group 的宽度决定。
    pub fn emit(self, packet: &mut [u8]) -> Result<usize, PacketError> {
        let width = if self.group.is_ipv4() { 4 } else { 16 };
        let length = 4 + width * (1 + usize::from(self.source_count));
        if packet.len() < length {
            return Err(PacketError::Capacity);
        }
        packet[..4].copy_from_slice(&[
            self.record_type as u8,
            0,
            (self.source_count >> 8) as u8,
            self.source_count as u8,
        ]);
        match self.group {
            IpAddr::V4(address) => packet[4..8].copy_from_slice(&address.octets()),
            IpAddr::V6(address) => packet[4..20].copy_from_slice(&address.octets()),
        }
        Ok(length)
    }
}

/// RFC 9776 的八位浮点编码解码；QQIC 单位为秒，IGMPv3 Max Resp Code 单位为 1/10 秒。
pub const fn decode_code8(code: u8) -> u32 {
    if code < 128 {
        code as u32
    } else {
        ((code as u32 & 15) | 16) << (((code >> 4) & 7) + 3)
    }
}

/// RFC 9777 的十六位 Maximum Response Code 解码，单位为毫秒；仅用于 MLDv2。
pub const fn decode_code16(code: u16) -> u32 {
    if code < 32768 {
        code as u32
    } else {
        ((code as u32 & 0xfff) | 0x1000) << (((code >> 12) & 7) + 3)
    }
}
