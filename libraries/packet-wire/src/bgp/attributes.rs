//! UPDATE 属性的完整 framing 与独立内容检查；不把坏属性直接等同于连接复位。
use super::nlri::{Family, Nlri};
use crate::PacketError;
use core::net::Ipv4Addr;

/// AS_PATH/AGGREGATOR 当前方向已协商的 AS 号宽度，不根据正文长度猜测。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AsnWidth {
    Two,
    Four,
}
impl AsnWidth {
    fn bytes(self) -> usize {
        match self {
            Self::Two => 2,
            Self::Four => 4,
        }
    }
}
/// 一个属性的原 flags、code 与不透明值；接收保留 flags 的低四位供上层原样留证。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Attribute<'a> {
    pub flags: u8,
    pub code: u8,
    pub data: &'a [u8],
}
/// 完整验证的属性游标；未知/重复属性保留，必需属性/重复属性/flags 的协议处置归会话 owner。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Attributes<'a> {
    bytes: &'a [u8],
}
impl<'a> Attributes<'a> {
    /// 检查短/扩展长度的全部属性范围，不在此拒绝单项内容错误。
    /// # Errors
    /// 头/长度/值截断。
    pub fn parse(bytes: &'a [u8]) -> Result<Self, PacketError> {
        for attribute in Self::scan(bytes) {
            attribute?;
        }
        Ok(Self { bytes })
    }
    /// 顺序检查未验证属性，先交出有效前缀，首次 framing 错误报告后停止。
    /// 这使协议 owner 能保留先到的 MP NLRI，再按 RFC 7606 处置后续坏属性，不能猜测重新同步。
    /// # Errors
    /// 迭代项可能为头/值截断；错误只报告一次，不再产生后续属性。
    pub fn scan(bytes: &'a [u8]) -> impl Iterator<Item = Result<Attribute<'a>, PacketError>> {
        Self::scan_wire(bytes).map(|result| result.map(|(attribute, _)| attribute))
    }
    /// 顺序交出属性及完整原 wire；错误通知必须引用原头/值，不能由协议状态层重建长度布局。
    /// # Errors
    /// 首次头/值截断报告一次后停止，已交出的前缀仍然有效。
    pub fn scan_wire(
        bytes: &'a [u8],
    ) -> impl Iterator<Item = Result<(Attribute<'a>, &'a [u8]), PacketError>> {
        let mut rest = bytes;
        core::iter::from_fn(move || {
            if rest.is_empty() {
                return None;
            }
            let result = (|| {
                let (head, length) = framing(rest)?;
                let data = rest
                    .get(head..head + length)
                    .ok_or(PacketError::Truncated)?;
                let attribute = Attribute {
                    flags: rest[0],
                    code: rest[1],
                    data,
                };
                let wire = &rest[..head + length];
                rest = &rest[head + length..];
                Ok((attribute, wire))
            })();
            if result.is_err() {
                rest = &[];
            }
            Some(result)
        })
    }
    /// 尚未迭代部分的原始属性字节。
    pub fn bytes(self) -> &'a [u8] {
        self.bytes
    }
    /// 在最终输出写属性序列，按值长度选择短/扩展长度并清 flags 低四位；返回实际长度。
    /// # Errors
    /// 值超过 u16、总长溢出或容量不足；失败不修改输出。语义准入由生成属性的协议 owner 负责。
    pub fn emit(bytes: &mut [u8], attributes: &[Attribute<'_>]) -> Result<usize, PacketError> {
        let length = attributes.iter().try_fold(0usize, |total, attribute| {
            total
                .checked_add(Attribute::encoded_len(attribute.data.len())?)
                .ok_or(PacketError::Malformed)
        })?;
        let bytes = bytes.get_mut(..length).ok_or(PacketError::Capacity)?;
        let mut at = 0;
        for attribute in attributes {
            at += Attribute::emit_with(
                &mut bytes[at..],
                attribute.flags,
                attribute.code,
                attribute.data.len(),
                |output| output.copy_from_slice(attribute.data),
            )
            .expect("attribute sizes prevalidated");
        }
        Ok(length)
    }
}
fn framing(bytes: &[u8]) -> Result<(usize, usize), PacketError> {
    let header = bytes.get(..3).ok_or(PacketError::Truncated)?;
    if header[0] & 0x10 != 0 {
        let low = *bytes.get(3).ok_or(PacketError::Truncated)?;
        Ok((4, usize::from(u16::from_be_bytes([header[2], low]))))
    } else {
        Ok((3, usize::from(header[2])))
    }
}
impl<'a> Iterator for Attributes<'a> {
    type Item = Attribute<'a>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.bytes.is_empty() {
            return None;
        }
        let (head, length) = framing(self.bytes).expect("attribute framing prevalidated");
        let attribute = Attribute {
            flags: self.bytes[0],
            code: self.bytes[1],
            data: &self.bytes[head..head + length],
        };
        self.bytes = &self.bytes[head + length..];
        Some(attribute)
    }
}

/// 已知属性的借用值；列表保留线上顺序，去重/属性驻留与最佳路径由 BGP owner 负责。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Value<'a> {
    Origin(u8),
    AsPath(AsPath<'a>),
    NextHop(Ipv4Addr),
    MultiExitDiscriminator(u32),
    LocalPreference(u32),
    AtomicAggregate,
    Aggregator { asn: u32, identifier: Ipv4Addr },
    Communities(&'a [u8]),
    OriginatorId(Ipv4Addr),
    ClusterList(&'a [u8]),
    MpReach(MpReach<'a>),
    MpUnreach(MpUnreach<'a>),
    ExtendedCommunities(&'a [u8]),
    LargeCommunities(&'a [u8]),
    Unknown(&'a [u8]),
}
fn word(bytes: &[u8]) -> Result<u32, PacketError> {
    Ok(u32::from_be_bytes(
        bytes.try_into().map_err(|_| PacketError::Malformed)?,
    ))
}
fn list(bytes: &[u8], width: usize) -> Result<&[u8], PacketError> {
    if bytes.is_empty() || !bytes.len().is_multiple_of(width) {
        return Err(PacketError::Malformed);
    }
    Ok(bytes)
}
impl<'a> Attribute<'a> {
    /// 已知属性值长度对应的完整 wire 长度，包括按需选择的短/扩展头。
    /// # Errors
    /// 值长度超过 u16；本函数不写输出。
    pub fn encoded_len(value_length: usize) -> Result<usize, PacketError> {
        u16::try_from(value_length).map_err(|_| PacketError::Malformed)?;
        Ok(value_length + if value_length > 255 { 4 } else { 3 })
    }
    /// 预检尺寸后，让无失败的值编码器直接写最终属性区间；清 flags 低四位。
    /// 调用者须预先验证值语义；write 只能写传入的精确长度区间，不需要中间值缓冲。
    /// # Errors
    /// 值超过 u16 或输出容量不足；出错不调用 write、不修改输出。
    pub fn emit_with(
        bytes: &mut [u8],
        flags: u8,
        code: u8,
        value_length: usize,
        write: impl FnOnce(&mut [u8]),
    ) -> Result<usize, PacketError> {
        let length = Self::encoded_len(value_length)?;
        let bytes = bytes.get_mut(..length).ok_or(PacketError::Capacity)?;
        let extended = value_length > 255;
        let head = if extended { 4 } else { 3 };
        bytes[0] = flags & 0xe0 | if extended { 0x10 } else { 0 };
        bytes[1] = code;
        if extended {
            bytes[2..4].copy_from_slice(&(value_length as u16).to_be_bytes());
        } else {
            bytes[2] = value_length as u8;
        }
        write(&mut bytes[head..]);
        Ok(length)
    }
    /// 指定方向聚合器的完整属性长度；基础/AS4 共用值布局，后者必须使用 Four。
    pub fn aggregator_len(width: AsnWidth) -> usize {
        3 + width.bytes() + 4
    }
    /// 向最终属性写基础或 AS4 聚合器；头/值的布局均由此格式 owner 负责。
    /// # Errors
    /// code 不是 7/18、AS4 不是四字节布局、两字节 AS 号无法表示或输出容量不足；失败不修改输出。
    pub fn emit_aggregator(
        bytes: &mut [u8],
        code: u8,
        width: AsnWidth,
        asn: u32,
        identifier: Ipv4Addr,
        partial: bool,
    ) -> Result<usize, PacketError> {
        if !matches!(code, 7 | 18)
            || code == 18 && width != AsnWidth::Four
            || width == AsnWidth::Two && asn > u32::from(u16::MAX)
        {
            return Err(PacketError::Malformed);
        }
        let width = width.bytes();
        Self::emit_with(
            bytes,
            if partial { 0xe0 } else { 0xc0 },
            code,
            width + 4,
            |output| {
                output[..width].copy_from_slice(&asn.to_be_bytes()[4 - width..]);
                output[width..].copy_from_slice(&identifier.octets());
            },
        )
    }
    /// 独立解码一个已知属性，保留未知属性及未解释的 AS4_PATH/AS4_AGGREGATOR 来源 code。
    /// # Parameters
    /// width 属于会话协商；AS4_PATH/AS4_AGGREGATOR 始终按四字节 AS 号解释，不猜测兼容布局。
    /// # Errors
    /// 已知属性的固定宽度/枚举/AS 段/MP framing 非法；不影响同 UPDATE 的其他属性或撤回区读取。
    pub fn decode(self, width: AsnWidth) -> Result<Value<'a>, PacketError> {
        Ok(match self.code {
            1 => match self.data {
                [value @ 0..=2] => Value::Origin(*value),
                _ => return Err(PacketError::Malformed),
            },
            2 | 17 => Value::AsPath(AsPath::parse(
                self.data,
                if self.code == 17 {
                    AsnWidth::Four
                } else {
                    width
                },
            )?),
            3 => Value::NextHop(Ipv4Addr::from(word(self.data)?)),
            4 => Value::MultiExitDiscriminator(word(self.data)?),
            5 => Value::LocalPreference(word(self.data)?),
            6 => {
                if !self.data.is_empty() {
                    return Err(PacketError::Malformed);
                }
                Value::AtomicAggregate
            }
            7 | 18 => {
                let width = if self.code == 18 {
                    AsnWidth::Four
                } else {
                    width
                }
                .bytes();
                if self.data.len() != width + 4 {
                    return Err(PacketError::Malformed);
                }
                let asn = if width == 2 {
                    u32::from(u16::from_be_bytes([self.data[0], self.data[1]]))
                } else {
                    word(&self.data[..4])?
                };
                Value::Aggregator {
                    asn,
                    identifier: Ipv4Addr::from(word(&self.data[width..])?),
                }
            }
            8 => Value::Communities(list(self.data, 4)?),
            9 => Value::OriginatorId(Ipv4Addr::from(word(self.data)?)),
            10 => Value::ClusterList(list(self.data, 4)?),
            14 => Value::MpReach(MpReach::parse(self.data)?),
            15 => Value::MpUnreach(MpUnreach::parse(self.data)?),
            16 => Value::ExtendedCommunities(list(self.data, 8)?),
            32 => Value::LargeCommunities(list(self.data, 12)?),
            _ => Value::Unknown(self.data),
        })
    }
}

/// 已检查分段范围的 AS_PATH；空路径合法，空段与未知段类型非法。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AsPath<'a> {
    bytes: &'a [u8],
    width: AsnWidth,
}
/// 段类型保留联盟段，不能在格式层擅自当成普通 AS_SEQUENCE。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum SegmentKind {
    Set,
    Sequence,
    ConfederationSequence,
    ConfederationSet,
}
impl SegmentKind {
    fn code(self) -> u8 {
        match self {
            Self::Set => 1,
            Self::Sequence => 2,
            Self::ConfederationSequence => 3,
            Self::ConfederationSet => 4,
        }
    }
}
fn segment_kind(value: u8) -> Result<SegmentKind, PacketError> {
    match value {
        1 => Ok(SegmentKind::Set),
        2 => Ok(SegmentKind::Sequence),
        3 => Ok(SegmentKind::ConfederationSequence),
        4 => Ok(SegmentKind::ConfederationSet),
        _ => Err(PacketError::Malformed),
    }
}
/// 一个 AS 段的类型与原值列表；没有 AS 号堆数组。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Segment<'a> {
    pub kind: SegmentKind,
    bytes: &'a [u8],
    width: AsnWidth,
}
impl<'a> AsPath<'a> {
    /// 完整验证指定协商宽度的 AS 段，不把路径长度推导成另一个 AS 宽度。
    /// # Errors
    /// 未知/空段、头或 AS 号截断。
    pub fn parse(bytes: &'a [u8], width: AsnWidth) -> Result<Self, PacketError> {
        let mut rest = bytes;
        while !rest.is_empty() {
            let header = rest.get(..2).ok_or(PacketError::Truncated)?;
            segment_kind(header[0])?;
            if header[1] == 0 {
                return Err(PacketError::Malformed);
            }
            rest = rest
                .get(2 + usize::from(header[1]) * width.bytes()..)
                .ok_or(PacketError::Truncated)?;
        }
        Ok(Self { bytes, width })
    }
    /// 借用原顺序 AS 段，不改变 set/sequence 的区别。
    pub fn segments(self) -> impl Iterator<Item = Segment<'a>> + Clone {
        let mut rest = self.bytes;
        core::iter::from_fn(move || {
            if rest.is_empty() {
                return None;
            }
            let end = 2 + usize::from(rest[1]) * self.width.bytes();
            let segment = Segment {
                kind: segment_kind(rest[0]).expect("validated segment type"),
                bytes: &rest[2..end],
                width: self.width,
            };
            rest = &rest[end..];
            Some(segment)
        })
    }
    /// 校验可重复遍历的段视图并计算值长度，不要求建立 AS 号数组。
    /// # Errors
    /// 段为空/超过 255 项、两字节布局无法表示 AS 号或长度溢出。
    pub fn encoded_len<S, A>(width: AsnWidth, segments: S) -> Result<usize, PacketError>
    where
        S: IntoIterator<Item = (SegmentKind, A)>,
        A: ExactSizeIterator<Item = u32>,
    {
        segments
            .into_iter()
            .try_fold(0usize, |total, (_, mut asns)| {
                let count = asns.len();
                if count == 0
                    || count > 255
                    || width == AsnWidth::Two && asns.any(|asn| asn > u32::from(u16::MAX))
                {
                    return Err(PacketError::Malformed);
                }
                total
                    .checked_add(2 + count * width.bytes())
                    .ok_or(PacketError::Malformed)
            })
    }
    /// 直接向最终属性值写可重复遍历的 AS 段；空路径合法，段顺序与类型保持。
    /// # Errors
    /// 段为空/超过 255 项、两字节布局无法表示 AS 号、长度溢出或容量不足；失败不修改输出。
    pub fn emit<S, A>(bytes: &mut [u8], width: AsnWidth, segments: S) -> Result<usize, PacketError>
    where
        S: Clone + IntoIterator<Item = (SegmentKind, A)>,
        A: ExactSizeIterator<Item = u32>,
    {
        let length = Self::encoded_len(width, segments.clone())?;
        let bytes = bytes.get_mut(..length).ok_or(PacketError::Capacity)?;
        let mut at = 0;
        for (kind, asns) in segments {
            bytes[at] = kind.code();
            bytes[at + 1] = asns.len() as u8;
            at += 2;
            for asn in asns {
                let encoded = asn.to_be_bytes();
                bytes[at..at + width.bytes()].copy_from_slice(&encoded[4 - width.bytes()..]);
                at += width.bytes();
            }
        }
        Ok(length)
    }
}
impl Segment<'_> {
    /// 将线上两/四字节 AS 号统一投影为 u32；数值 0/AS_TRANS 的接纳由协议 owner 决定。
    pub fn asns(self) -> impl ExactSizeIterator<Item = u32> + Clone {
        self.bytes.chunks_exact(self.width.bytes()).map(|bytes| {
            if bytes.len() == 2 {
                u32::from(u16::from_be_bytes([bytes[0], bytes[1]]))
            } else {
                u32::from_be_bytes(bytes.try_into().expect("checked AS width"))
            }
        })
    }
}

/// MP_REACH 的地址族/SAFI、原始下一跳和 NLRI；下一跳族/链路范围还需按协商检查。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MpReach<'a> {
    pub afi: u16,
    pub safi: u8,
    pub next_hop: &'a [u8],
    nlri: &'a [u8],
}
/// MP_UNREACH 保留空 NLRI 的 End-of-RIB 表示，不与整个 UPDATE 空消息混同。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MpUnreach<'a> {
    pub afi: u16,
    pub safi: u8,
    nlri: &'a [u8],
}
fn prefixes(afi: u16, safi: u8, bytes: &[u8], path_ids: bool) -> Result<Nlri<'_>, PacketError> {
    if !matches!(safi, 1 | 2) {
        return Err(PacketError::Unsupported);
    }
    let family = match afi {
        1 => Family::Ipv4,
        2 => Family::Ipv6,
        _ => return Err(PacketError::Unsupported),
    };
    Nlri::parse_with_path_ids(bytes, family, path_ids)
}
fn afi(prefixes: Nlri<'_>, safi: u8) -> Result<u16, PacketError> {
    if !matches!(safi, 1 | 2) {
        return Err(PacketError::Unsupported);
    }
    Ok(match prefixes.family() {
        Family::Ipv4 => 1,
        Family::Ipv6 => 2,
    })
}
impl<'a> MpReach<'a> {
    fn parse(bytes: &'a [u8]) -> Result<Self, PacketError> {
        let header = bytes.get(..4).ok_or(PacketError::Truncated)?;
        let end = 4 + usize::from(header[3]);
        let next_hop = bytes.get(4..end).ok_or(PacketError::Truncated)?;
        let nlri = bytes.get(end + 1..).ok_or(PacketError::Truncated)?;
        Ok(Self {
            afi: u16::from_be_bytes([header[0], header[1]]),
            safi: header[2],
            next_hop,
            nlri,
        })
    }
    /// 检查两族普通 unicast/multicast 前缀；未知族/SAFI 不套用错误的 NLRI 布局。
    /// # Errors
    /// 族/SAFI 不支持或 NLRI 非法；被保留的下一跳字节仍可由相应扩展 owner 检查。
    pub fn prefixes(self) -> Result<Nlri<'a>, PacketError> {
        self.prefixes_with_path_ids(false)
    }
    /// 检查本方向的 MP 宣告，路径标识的布局来自协商。
    /// # Errors
    /// 族/SAFI 不支持，或 NLRI 非法。
    pub fn prefixes_with_path_ids(self, path_ids: bool) -> Result<Nlri<'a>, PacketError> {
        prefixes(self.afi, self.safi, self.nlri, path_ids)
    }
    /// 将普通双栈前缀与已由协议 owner 按协商检查的下一跳写入最终属性值，清零 reserved。
    /// # Errors
    /// SAFI 不支持、下一跳长度超出 u8、总长溢出或容量不足；失败不修改输出。
    pub fn emit(
        bytes: &mut [u8],
        safi: u8,
        next_hop: &[u8],
        prefixes: Nlri<'_>,
    ) -> Result<usize, PacketError> {
        let afi = afi(prefixes, safi)?;
        let hop_length = u8::try_from(next_hop.len()).map_err(|_| PacketError::Malformed)?;
        let length = (5 + next_hop.len())
            .checked_add(prefixes.bytes().len())
            .ok_or(PacketError::Malformed)?;
        let bytes = bytes.get_mut(..length).ok_or(PacketError::Capacity)?;
        bytes[..2].copy_from_slice(&afi.to_be_bytes());
        bytes[2] = safi;
        bytes[3] = hop_length;
        bytes[4..4 + next_hop.len()].copy_from_slice(next_hop);
        bytes[4 + next_hop.len()] = 0;
        bytes[5 + next_hop.len()..].copy_from_slice(prefixes.bytes());
        Ok(length)
    }
}
impl<'a> MpUnreach<'a> {
    fn parse(bytes: &'a [u8]) -> Result<Self, PacketError> {
        let header = bytes.get(..3).ok_or(PacketError::Truncated)?;
        Ok(Self {
            afi: u16::from_be_bytes([header[0], header[1]]),
            safi: header[2],
            nlri: &bytes[3..],
        })
    }
    /// 检查普通双栈前缀；空列表保留给状态机作为 End-of-RIB。
    /// # Errors
    /// 族/SAFI 不支持或 NLRI 非法。
    pub fn prefixes(self) -> Result<Nlri<'a>, PacketError> {
        self.prefixes_with_path_ids(false)
    }
    /// 检查本方向的 MP 撤回，路径标识的布局来自协商。
    /// # Errors
    /// 族/SAFI 不支持，或 NLRI 非法。
    pub fn prefixes_with_path_ids(self, path_ids: bool) -> Result<Nlri<'a>, PacketError> {
        prefixes(self.afi, self.safi, self.nlri, path_ids)
    }
    /// 写普通双栈撤回列表；空列表可用于该族的 End-of-RIB。
    /// # Errors
    /// SAFI 不支持、总长溢出或容量不足；失败不修改输出。
    pub fn emit(bytes: &mut [u8], safi: u8, prefixes: Nlri<'_>) -> Result<usize, PacketError> {
        let afi = afi(prefixes, safi)?;
        let length = prefixes
            .bytes()
            .len()
            .checked_add(3)
            .ok_or(PacketError::Malformed)?;
        let bytes = bytes.get_mut(..length).ok_or(PacketError::Capacity)?;
        bytes[..2].copy_from_slice(&afi.to_be_bytes());
        bytes[2] = safi;
        bytes[3..].copy_from_slice(prefixes.bytes());
        Ok(length)
    }
}
