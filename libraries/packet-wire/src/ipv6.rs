//! RFC 8200 IPv6 与扩展头链；RFC 2675 Jumbo Payload。只解释格式，不执行路由、重组或认证。
pub mod options;
use crate::PacketError;
use core::net::Ipv6Addr;

/// 格式错误及其在 IPv6 包中的字段位置；协议 owner 决定对应的 ICMP 响应和限速。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ipv6Error {
    pub kind: PacketError,
    pub offset: usize,
    /// 非原子首分片缺失后续扩展头，不能排队等待其余分片补全这一 header chain。
    pub incomplete_first_fragment: bool,
}
fn error(kind: PacketError, offset: usize) -> Ipv6Error {
    Ipv6Error {
        kind,
        offset,
        incomplete_first_fragment: false,
    }
}

/// 固定头与可见扩展头链均已验证的 IPv6 借用；分片仍交给唯一重组 owner。
#[derive(Clone, Copy)]
pub struct Ipv6Packet<'a> {
    bytes: &'a [u8],
    chain: Chain,
}
#[derive(Clone, Copy)]
struct Chain {
    end: usize,
    protocol: u8,
    next_field: usize,
    // 非原子分片不能直接作为 transport；缺少此事实会把中间分片当作新 UDP/TCP 头。
    fragmented: bool,
}
impl<'a> Ipv6Packet<'a> {
    /// 解析完整 IP 包并排除链路 padding；非首分片只解析到 Fragment header，不解读其数据。
    /// # Errors
    /// 截断、版本/长度/扩展头格式错误；保留错误字段位置及不完整首分片信息。
    pub fn parse(bytes: &'a [u8]) -> Result<Self, Ipv6Error> {
        if bytes.len() < 40 {
            return Err(error(PacketError::Truncated, bytes.len()));
        }
        if bytes[0] >> 4 != 6 {
            return Err(error(PacketError::Unsupported, 0));
        }
        let declared = usize::from(u16::from_be_bytes([bytes[4], bytes[5]]));
        // 首个 HBH 的解析事实在同一不可变字节上重用，Jumbo 长度发现不重复扫描该 TLV 区。
        let hop = if bytes[6] == 0 {
            let bounded = if declared == 0 {
                bytes
            } else {
                bytes
                    .get(..40 + declared)
                    .ok_or_else(|| error(PacketError::Truncated, 4))?
            };
            Some(hop_facts(bounded, 40, 0)?)
        } else {
            None
        };
        let length = match (declared, hop.as_ref().and_then(|facts| facts.jumbo)) {
            (0, Some((length, _))) => length as usize,
            (0, None) if bytes[6] == 0 => return Err(error(PacketError::Malformed, 4)),
            (_, Some((_, offset))) => return Err(error(PacketError::Malformed, offset)),
            (length, None) => length,
        };
        let end = 40usize
            .checked_add(length)
            .ok_or_else(|| error(PacketError::Malformed, 4))?;
        let bytes = bytes
            .get(..end)
            .ok_or_else(|| error(PacketError::Truncated, 4))?;
        let chain = scan(bytes, bytes[6], length > 65535, hop)?;
        Ok(Self { bytes, chain })
    }
    /// 排除链路 padding 的完整 IP 字节。
    pub fn bytes(self) -> &'a [u8] {
        self.bytes
    }
    /// 包含扩展头的原始 IPv6 payload。
    pub fn payload(self) -> &'a [u8] {
        &self.bytes[40..]
    }
    /// 固定头源地址。
    pub fn source(self) -> Ipv6Addr {
        Ipv6Addr::from(<[u8; 16]>::try_from(&self.bytes[8..24]).expect("validated header"))
    }
    /// 固定头当前目的地址；Routing header 的最终目的地址须由路由类型 owner 决定，再构造传输伪头。
    pub fn destination(self) -> Ipv6Addr {
        Ipv6Addr::from(<[u8; 16]>::try_from(&self.bytes[24..40]).expect("validated header"))
    }
    /// Traffic Class 的完整 8 位。
    pub fn traffic_class(self) -> u8 {
        self.bytes[0] << 4 | self.bytes[1] >> 4
    }
    /// 20-bit flow label。
    pub fn flow_label(self) -> u32 {
        u32::from_be_bytes(self.bytes[..4].try_into().expect("validated header")) & 0x000f_ffff
    }
    /// Hop Limit 原值，转发/本机语义由引擎决定。
    pub fn hop_limit(self) -> u8 {
        self.bytes[7]
    }
    /// 扩展链终止时的协议；ESP/未知协议是 opaque terminal，不猜测加密后的 Next Header。
    pub fn upper_protocol(self) -> u8 {
        self.chain.protocol
    }
    /// 指向 upper_protocol 的 Next Header 字段位置，供未知协议的 Parameter Problem 使用。
    pub fn next_header_offset(self) -> usize {
        self.chain.next_field
    }
    /// 扩展链之后的位置；非首分片这里是分片数据，不是 transport header。
    pub fn upper_offset(self) -> usize {
        self.chain.end
    }
    /// 是否必须经过重组；atomic fragment 不设置此状态。
    pub fn needs_reassembly(self) -> bool {
        self.chain.fragmented
    }
    /// 完整 transport/opaque protocol 数据；未重组或 No Next Header 返回 None。
    pub fn upper_layer(self) -> Option<&'a [u8]> {
        (!self.chain.fragmented && self.chain.protocol != 59)
            .then_some(&self.bytes[self.chain.end..])
    }
    /// 只遍历构造时已验证的扩展头，不对非首分片数据继续解码。
    pub fn extensions(self) -> Extensions<'a> {
        Extensions {
            bytes: &self.bytes[40..self.chain.end],
            kind: self.bytes[6],
            offset: 40,
        }
    }
}

struct HopFacts {
    length: usize,
    jumbo: Option<(u32, usize)>,
}
fn hop_facts(bytes: &[u8], offset: usize, kind: u8) -> Result<HopFacts, Ipv6Error> {
    let length = extension_length(bytes, offset, kind)?;
    let mut jumbo = None;
    for option in options::Options::parse(&bytes[offset + 2..offset + length], offset + 2)? {
        if option.kind != 0xc2 {
            continue;
        }
        if kind != 0 || option.data.len() != 4 || option.offset % 4 != 2 || jumbo.is_some() {
            return Err(error(PacketError::Malformed, option.offset));
        }
        let value = u32::from_be_bytes(option.data.try_into().expect("four-byte jumbo option"));
        if value <= 65535 {
            return Err(error(PacketError::Malformed, option.offset + 2));
        }
        jumbo = Some((value, option.offset));
    }
    Ok(HopFacts { length, jumbo })
}
fn extension_length(bytes: &[u8], offset: usize, kind: u8) -> Result<usize, Ipv6Error> {
    let header = bytes
        .get(offset..offset + 2)
        .ok_or_else(|| error(PacketError::Truncated, offset))?;
    let length = match kind {
        44 => 8,
        51 => (usize::from(header[1]) + 2) * 4,
        _ => (usize::from(header[1]) + 1) * 8,
    };
    if kind == 51 && (length < 12 || !length.is_multiple_of(8)) {
        return Err(error(PacketError::Malformed, offset + 1));
    }
    if length > bytes.len() - offset {
        return Err(error(PacketError::Truncated, offset + 1));
    }
    Ok(length)
}
fn scan(
    bytes: &[u8],
    first: u8,
    jumbo: bool,
    mut hop: Option<HopFacts>,
) -> Result<Chain, Ipv6Error> {
    let mut chain = Chain {
        end: 40,
        protocol: first,
        next_field: 6,
        fragmented: false,
    };
    loop {
        let kind = chain.protocol;
        if !matches!(kind, 0 | 43 | 44 | 51 | 60) {
            break;
        }
        let offset = chain.end;
        if kind == 0 && offset != 40 {
            return Err(error(PacketError::Unsupported, chain.next_field));
        }
        let inspected = match kind {
            0 => hop
                .take()
                .map(Ok)
                .unwrap_or_else(|| hop_facts(bytes, offset, kind))
                .map(|facts| facts.length),
            60 => hop_facts(bytes, offset, kind).map(|facts| facts.length),
            _ => extension_length(bytes, offset, kind),
        };
        let length = inspected.map_err(|mut problem| {
            problem.incomplete_first_fragment =
                chain.fragmented && problem.kind == PacketError::Truncated;
            problem
        })?;
        if kind == 44 {
            if jumbo {
                return Err(error(PacketError::Malformed, offset));
            }
            let fragment = FragmentHeader {
                bytes: &bytes[offset..offset + 8],
                offset,
            };
            let payload = bytes.len() - offset - 8;
            if fragment.more() && !payload.is_multiple_of(8) {
                return Err(error(PacketError::Malformed, 4));
            }
            if fragment.fragment_offset() + payload + offset - 40 > 65535 {
                return Err(error(PacketError::Malformed, offset + 2));
            }
            chain.fragmented |= fragment.fragment_offset() != 0 || fragment.more();
            chain.protocol = bytes[offset];
            chain.next_field = offset;
            chain.end += 8;
            if fragment.fragment_offset() != 0 {
                break;
            }
        } else {
            chain.protocol = bytes[offset];
            chain.next_field = offset;
            chain.end += length;
        }
    }
    Ok(chain)
}

/// 已验证的扩展头字段；kind/offset 保留原序与位置，未知 Routing type 不在 codec 内处理。
#[derive(Clone, Copy)]
pub struct ExtensionHeader<'a> {
    kind: u8,
    offset: usize,
    bytes: &'a [u8],
}
impl<'a> ExtensionHeader<'a> {
    /// 该扩展头的协议号。
    pub fn kind(self) -> u8 {
        self.kind
    }
    /// 在完整 IPv6 包中的位置。
    pub fn offset(self) -> usize {
        self.offset
    }
    /// 完整扩展头字节。
    pub fn bytes(self) -> &'a [u8] {
        self.bytes
    }
    /// 后继协议号。
    pub fn next_header(self) -> u8 {
        self.bytes[0]
    }
    /// Hop/Destination options；其他扩展头返回 None。
    pub fn options(self) -> Option<options::Options<'a>> {
        matches!(self.kind, 0 | 60)
            .then(|| options::Options::validated(&self.bytes[2..], self.offset + 2))
    }
    /// Fragment header 的类型化字段；其他扩展头返回 None。
    pub fn fragment(self) -> Option<FragmentHeader<'a>> {
        (self.kind == 44).then_some(FragmentHeader {
            bytes: self.bytes,
            offset: self.offset,
        })
    }
}
/// 已验证扩展链的一次线性遍历；不分配 header Vec。
pub struct Extensions<'a> {
    bytes: &'a [u8],
    kind: u8,
    offset: usize,
}
impl<'a> Iterator for Extensions<'a> {
    type Item = ExtensionHeader<'a>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.bytes.is_empty() {
            return None;
        }
        let length = match self.kind {
            44 => 8,
            51 => (usize::from(self.bytes[1]) + 2) * 4,
            _ => (usize::from(self.bytes[1]) + 1) * 8,
        };
        let header = ExtensionHeader {
            kind: self.kind,
            offset: self.offset,
            bytes: &self.bytes[..length],
        };
        self.kind = header.next_header();
        self.offset += length;
        self.bytes = &self.bytes[length..];
        Some(header)
    }
}
/// Fragment header 借用；接收时忽略 reserved 位，发送者必须清零。
#[derive(Clone, Copy)]
pub struct FragmentHeader<'a> {
    bytes: &'a [u8],
    offset: usize,
}
impl FragmentHeader<'_> {
    /// 以字节为单位的分片偏移。
    pub fn fragment_offset(self) -> usize {
        usize::from(u16::from_be_bytes([self.bytes[2], self.bytes[3]]) & 0xfff8)
    }
    /// M flag。
    pub fn more(self) -> bool {
        self.bytes[3] & 1 != 0
    }
    /// 完整 32-bit Identification。
    pub fn identification(self) -> u32 {
        u32::from_be_bytes(self.bytes[4..8].try_into().expect("validated fragment"))
    }
    /// 此 Fragment header 在完整 IP 包中的位置。
    pub fn header_offset(self) -> usize {
        self.offset
    }
}

/// 固定 IPv6 header 的输出字段；扩展头和 payload 已位于 output[40..]。
pub struct Ipv6Header {
    pub source: Ipv6Addr,
    pub destination: Ipv6Addr,
    pub traffic_class: u8,
    pub flow_label: u32,
    pub next_header: u8,
    pub hop_limit: u8,
}
impl Ipv6Header {
    /// 预先校验扩展链/长度后原位写固定头，保留既有 payload。
    /// # Errors
    /// 空间、flow label、Jumbo/分片或扩展链不合法，写入前返回错误。
    pub fn emit(&self, output: &mut [u8]) -> Result<(), PacketError> {
        if output.len() < 40 || output.len() - 40 > u32::MAX as usize {
            return Err(PacketError::Capacity);
        }
        if self.flow_label > 0x000f_ffff {
            return Err(PacketError::Malformed);
        }
        let length = output.len() - 40;
        let hop = if self.next_header == 0 {
            Some(hop_facts(output, 40, 0).map_err(|problem| problem.kind)?)
        } else {
            None
        };
        match (length > 65535, hop.as_ref().and_then(|facts| facts.jumbo)) {
            (true, Some((value, _))) if value as usize == length => (),
            (false, None) => (),
            _ => return Err(PacketError::Malformed),
        }
        scan(output, self.next_header, length > 65535, hop).map_err(|problem| problem.kind)?;
        let first = (6u32 << 28) | u32::from(self.traffic_class) << 20 | self.flow_label;
        output[..4].copy_from_slice(&first.to_be_bytes());
        output[4..6]
            .copy_from_slice(&(if length > 65535 { 0 } else { length as u16 }).to_be_bytes());
        output[6] = self.next_header;
        output[7] = self.hop_limit;
        output[8..24].copy_from_slice(&self.source.octets());
        output[24..40].copy_from_slice(&self.destination.octets());
        Ok(())
    }
}

/// 输出一个 Fragment header；offset 使用字节，必须为 8 的倍数，不负责发送分片决策。
pub struct Fragment {
    pub next_header: u8,
    pub offset: usize,
    pub more: bool,
    pub identification: u32,
}
impl Fragment {
    /// 填写 output 的前 8 字节并清零 reserved 位。
    /// # Errors
    /// 空间不足或 offset 不能由 13-bit 单位偏移表示；错误不写入。
    pub fn emit(&self, output: &mut [u8]) -> Result<(), PacketError> {
        if self.offset > 65528 || !self.offset.is_multiple_of(8) {
            return Err(PacketError::Malformed);
        }
        let output = output.get_mut(..8).ok_or(PacketError::Capacity)?;
        output[0] = self.next_header;
        output[1] = 0;
        output[2..4].copy_from_slice(&(self.offset as u16 | u16::from(self.more)).to_be_bytes());
        output[4..8].copy_from_slice(&self.identification.to_be_bytes());
        Ok(())
    }
}

/// Routing header 的格式字段；data 是路由类型专属部分，包含该类型要求的 reserved/address 字节。
pub struct Routing<'a> {
    pub next_header: u8,
    pub routing_type: u8,
    pub segments_left: u8,
    pub data: &'a [u8],
}
impl Routing<'_> {
    /// 向 output 前部编码完整 Routing header，返回长度；不判断路由类型是否获准执行。
    /// # Errors
    /// 不是 8-byte 对齐、头长不足/超过 Hdr Ext Len 范围或输出不足；失败不写入。
    pub fn emit(&self, output: &mut [u8]) -> Result<usize, PacketError> {
        let length = self
            .data
            .len()
            .checked_add(4)
            .ok_or(PacketError::Capacity)?;
        if !(8..=2048).contains(&length) || !length.is_multiple_of(8) {
            return Err(PacketError::Malformed);
        }
        let output = output.get_mut(..length).ok_or(PacketError::Capacity)?;
        output[0] = self.next_header;
        output[1] = (length / 8 - 1) as u8;
        output[2] = self.routing_type;
        output[3] = self.segments_left;
        output[4..].copy_from_slice(self.data);
        Ok(length)
    }
}

/// RFC 4302 AH 的格式字段；authentication 是算法 owner 已生成的 ICV，不在此执行认证。
pub struct Authentication<'a> {
    pub next_header: u8,
    pub spi: u32,
    pub sequence: u32,
    pub authentication: &'a [u8],
}
impl Authentication<'_> {
    /// 编码 IPv6 AH 并清零 reserved 和 8-byte 对齐 padding，返回完整头长。
    /// # Errors
    /// 长度/输出容量超限；失败不写入。
    pub fn emit(&self, output: &mut [u8]) -> Result<usize, PacketError> {
        let used = self
            .authentication
            .len()
            .checked_add(12)
            .ok_or(PacketError::Capacity)?;
        let length = used.checked_add(7).ok_or(PacketError::Capacity)? & !7;
        if length > 1024 {
            return Err(PacketError::Capacity);
        }
        let output = output.get_mut(..length).ok_or(PacketError::Capacity)?;
        output[0] = self.next_header;
        output[1] = (length / 4 - 2) as u8;
        output[2..4].fill(0);
        output[4..8].copy_from_slice(&self.spi.to_be_bytes());
        output[8..12].copy_from_slice(&self.sequence.to_be_bytes());
        output[12..used].copy_from_slice(self.authentication);
        output[used..].fill(0);
        Ok(length)
    }
}
