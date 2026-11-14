//! UPDATE 区间独立捕获，避免一个坏属性阻止 RFC 7606 对仍可读的撤回/NLRI 做分类处置。
use super::{
    HEADER_BYTES, Kind,
    attributes::{Attribute, Attributes},
    nlri::{Family, Nlri},
};
use crate::PacketError;

/// 两个区间长度已检查的 UPDATE；前缀/属性各自验证，没有中间前缀或属性数组。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Update<'a> {
    withdrawn: &'a [u8],
    attributes: &'a [u8],
    announced: &'a [u8],
}
impl<'a> Update<'a> {
    /// 在最终 UPDATE 的属性区直接生成属性，避免协议 owner 复制字段布局或中间属性正文。
    /// withdrawn/announced 必须为 IPv4；生产者生成合法属性并返回实际长度，所有区间仍由本 codec 装配。
    /// # Errors
    /// 族、长度或容量非法，或生产者失败；失败时输出内容未定义，不得发送。
    pub fn emit_with(
        bytes: &mut [u8],
        withdrawn: Nlri<'_>,
        announced: Nlri<'_>,
        extended: bool,
        attributes: impl FnOnce(&mut [u8]) -> Result<usize, PacketError>,
    ) -> Result<usize, PacketError> {
        if withdrawn.family() != Family::Ipv4 || announced.family() != Family::Ipv4 {
            return Err(PacketError::Malformed);
        }
        let withdrawn = withdrawn.bytes();
        let announced = announced.bytes();
        let limit = if extended {
            usize::from(u16::MAX)
        } else {
            super::STANDARD_LIMIT
        };
        let capacity = bytes.len().min(limit);
        let fixed = (HEADER_BYTES + 4)
            .checked_add(withdrawn.len())
            .and_then(|size| size.checked_add(announced.len()))
            .ok_or(PacketError::Malformed)?;
        if fixed > capacity {
            return Err(PacketError::Capacity);
        }
        let start = HEADER_BYTES + 4 + withdrawn.len();
        let length = attributes(&mut bytes[start..capacity - announced.len()])?;
        let total = fixed.checked_add(length).ok_or(PacketError::Malformed)?;
        if total > capacity {
            return Err(PacketError::Capacity);
        }
        let attribute_length = u16::try_from(length).map_err(|_| PacketError::Malformed)?;
        let withdrawn_length =
            u16::try_from(withdrawn.len()).map_err(|_| PacketError::Malformed)?;
        bytes[19..21].copy_from_slice(&withdrawn_length.to_be_bytes());
        bytes[21..21 + withdrawn.len()].copy_from_slice(withdrawn);
        bytes[21 + withdrawn.len()..start].copy_from_slice(&attribute_length.to_be_bytes());
        bytes[start + length..total].copy_from_slice(announced);
        super::emit(&mut bytes[..total], Kind::Update, extended)?;
        Ok(total)
    }
    /// 检查共同头之后的 UPDATE 固定长度与两个区间边界。
    /// # Errors
    /// 长度字段截断或声明区间超出消息。
    pub fn parse(bytes: &'a [u8]) -> Result<Self, PacketError> {
        let header = bytes.get(..2).ok_or(PacketError::Truncated)?;
        let withdrawn_length = usize::from(u16::from_be_bytes([header[0], header[1]]));
        let withdrawn = bytes
            .get(2..2 + withdrawn_length)
            .ok_or(PacketError::Truncated)?;
        let bytes = &bytes[2 + withdrawn_length..];
        let header = bytes.get(..2).ok_or(PacketError::Truncated)?;
        let length = usize::from(u16::from_be_bytes([header[0], header[1]]));
        let attributes = bytes.get(2..2 + length).ok_or(PacketError::Truncated)?;
        Ok(Self {
            withdrawn,
            attributes,
            announced: &bytes[2 + length..],
        })
    }
    /// 检查基础 IPv4 撤回区；不受属性内容错误影响。
    /// # Errors
    /// 非法/截断前缀。
    pub fn withdrawn(self) -> Result<Nlri<'a>, PacketError> {
        self.withdrawn_with_path_ids(false)
    }
    /// 检查基础 IPv4 撤回区，使用本方向已协商的路径标识布局。
    /// # Errors
    /// 前缀或路径标识非法或截断。
    pub fn withdrawn_with_path_ids(self, path_ids: bool) -> Result<Nlri<'a>, PacketError> {
        Nlri::parse_with_path_ids(self.withdrawn, Family::Ipv4, path_ids)
    }
    /// 检查属性 framing，不把单项内容错误升级为整个 UPDATE framing 错误。
    /// # Errors
    /// 属性头/值越界。
    pub fn attributes(self) -> Result<Attributes<'a>, PacketError> {
        Attributes::parse(self.attributes)
    }
    /// 逐项检查属性，在后续 framing 错误前仍交出先到的 MP NLRI；不能因此跳过 RFC 7606 的错误裁决。
    /// # Errors
    /// 迭代项报告首次属性头/值越界并终止；撤回与基础 NLRI 区间仍各自可读。
    pub fn scan_attributes(self) -> impl Iterator<Item = Result<Attribute<'a>, PacketError>> {
        Attributes::scan(self.attributes)
    }
    /// 顺序交出有效属性和原始头/值，供错误通知留证；首次结构错误后停止。
    /// # Errors
    /// 迭代项报告属性头/值截断；不会失去之前已经辨认的 MP 属性。
    pub fn scan_wire_attributes(
        self,
    ) -> impl Iterator<Item = Result<(Attribute<'a>, &'a [u8]), PacketError>> {
        Attributes::scan_wire(self.attributes)
    }
    /// 检查基础 IPv4 宣告区，MP 地址族保留在其对应属性内。
    /// # Errors
    /// 非法/截断前缀。
    pub fn announced(self) -> Result<Nlri<'a>, PacketError> {
        self.announced_with_path_ids(false)
    }
    /// 检查基础 IPv4 宣告区，使用本方向已协商的路径标识布局。
    /// # Errors
    /// 前缀或路径标识非法或截断。
    pub fn announced_with_path_ids(self, path_ids: bool) -> Result<Nlri<'a>, PacketError> {
        Nlri::parse_with_path_ids(self.announced, Family::Ipv4, path_ids)
    }
    /// 编码经过各自 framing 检查的区间到最终消息，返回实际长度；每段仅复制一次。
    /// # Errors
    /// 基础区间不是 IPv4、区间超出 u16、消息超出已授权上限或容量不足；失败不修改输出。
    pub fn emit(
        bytes: &mut [u8],
        withdrawn: Nlri<'_>,
        attributes: Attributes<'_>,
        announced: Nlri<'_>,
        extended: bool,
    ) -> Result<usize, PacketError> {
        if withdrawn.family() != Family::Ipv4 || announced.family() != Family::Ipv4 {
            return Err(PacketError::Malformed);
        }
        let withdrawn = withdrawn.bytes();
        let attributes = attributes.bytes();
        let announced = announced.bytes();
        let withdrawn_length =
            u16::try_from(withdrawn.len()).map_err(|_| PacketError::Malformed)?;
        let attribute_length =
            u16::try_from(attributes.len()).map_err(|_| PacketError::Malformed)?;
        let total = (HEADER_BYTES + 4)
            .checked_add(withdrawn.len())
            .and_then(|size| size.checked_add(attributes.len()))
            .and_then(|size| size.checked_add(announced.len()))
            .ok_or(PacketError::Malformed)?;
        if !super::valid_length(Kind::Update, total, extended) {
            return Err(PacketError::Malformed);
        }
        let bytes = bytes.get_mut(..total).ok_or(PacketError::Capacity)?;
        bytes[19..21].copy_from_slice(&withdrawn_length.to_be_bytes());
        bytes[21..21 + withdrawn.len()].copy_from_slice(withdrawn);
        let at = 21 + withdrawn.len();
        bytes[at..at + 2].copy_from_slice(&attribute_length.to_be_bytes());
        bytes[at + 2..at + 2 + attributes.len()].copy_from_slice(attributes);
        bytes[at + 2 + attributes.len()..].copy_from_slice(announced);
        super::emit(bytes, Kind::Update, extended).expect("UPDATE message size prevalidated");
        Ok(total)
    }
}
