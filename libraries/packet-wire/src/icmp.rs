//! RFC 4884 的跨地址族错误引用区与扩展对象；只拥有字节边界，不拥有错误投递或 traceroute 状态。
use crate::{PacketError, checksum::checksum};

/// 错误消息的引用与可选扩展；引用可截断且包含 padding，不要求原 IP 数据报完整。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ErrorBody<'a> {
    bytes: &'a [u8],
    quoted_length: usize,
}
impl<'a> ErrorBody<'a> {
    pub(crate) fn new(bytes: &'a [u8], quoted_length: usize) -> Self {
        Self {
            bytes,
            quoted_length,
        }
    }
    /// 读取引用原包及 padding；长度属性为零时整个 body 都是引用，不猜测旧实现的扩展位置。
    /// # Errors
    /// 非零引用长度小于 128 或超出消息；扩展本身损坏不妨碍读取有明确边界的引用。
    pub fn quoted_bytes(self) -> Result<&'a [u8], PacketError> {
        if self.quoted_length == 0 {
            return Ok(self.bytes);
        }
        if self.quoted_length < 128 {
            return Err(PacketError::Malformed);
        }
        self.bytes
            .get(..self.quoted_length)
            .ok_or(PacketError::Truncated)
    }
    /// 单独验证扩展结构；失败不使已验证的外层 ICMP 信封或引用区失效。
    /// # Errors
    /// 引用边界、版本/保留字段、checksum 或对象长度非法。
    pub fn extensions(self) -> Result<Option<Extensions<'a>>, PacketError> {
        if self.quoted_length == 0 {
            return Ok(None);
        }
        self.quoted_bytes()?;
        Extensions::parse(&self.bytes[self.quoted_length..]).map(Some)
    }
}

/// 完整验证的版本 2 扩展结构；未知 Class/C-Type 和重复对象原样保留。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Extensions<'a> {
    bytes: &'a [u8],
}
impl<'a> Extensions<'a> {
    /// 验证头、可选 checksum 和所有对象范围，至少需要一个对象。
    /// # Errors
    /// 截断、非版本 2、保留位非零、checksum 错误或非 32-bit 对齐的对象长度。
    pub fn parse(bytes: &'a [u8]) -> Result<Self, PacketError> {
        let header = bytes.get(..4).ok_or(PacketError::Truncated)?;
        if header[0] >> 4 != 2 {
            return Err(PacketError::Unsupported);
        }
        if header[0] & 15 != 0 || header[1] != 0 {
            return Err(PacketError::Malformed);
        }
        // RFC 4884 的零字段明确表示未传 checksum，不能据此拒绝合法扩展。
        if header[2..4] != [0; 2] && checksum(bytes) != 0 {
            return Err(PacketError::Checksum);
        }
        let mut rest = &bytes[4..];
        if rest.is_empty() {
            return Err(PacketError::Malformed);
        }
        while !rest.is_empty() {
            let header = rest.get(..4).ok_or(PacketError::Truncated)?;
            let length = usize::from(u16::from_be_bytes([header[0], header[1]]));
            if length < 4 || !length.is_multiple_of(4) {
                return Err(PacketError::Malformed);
            }
            rest = rest.get(length..).ok_or(PacketError::Truncated)?;
        }
        Ok(Self { bytes })
    }
    /// 完整结构字节，包括扩展头与 checksum。
    pub fn bytes(self) -> &'a [u8] {
        self.bytes
    }
    /// 无分配读取原顺序对象；范围已由 parse 一次检查，不重复扫描对象正文。
    pub fn objects(self) -> impl Iterator<Item = Object<'a>> {
        let mut rest = &self.bytes[4..];
        core::iter::from_fn(move || {
            if rest.is_empty() {
                return None;
            }
            let length = usize::from(u16::from_be_bytes([rest[0], rest[1]]));
            let object = Object {
                class: rest[2],
                subtype: rest[3],
                data: &rest[4..length],
            };
            rest = &rest[length..];
            Some(object)
        })
    }
    /// 在最终输出中依次写头、对象及 checksum；返回实际长度，余下容量不改变。
    /// # Errors
    /// 没有对象、对象非四字节对齐/超出 u16、总长溢出或容量不足；失败不改输出。
    pub fn emit(bytes: &mut [u8], objects: &[Object<'_>]) -> Result<usize, PacketError> {
        let length = encoded_length(objects)?;
        let output = bytes.get_mut(..length).ok_or(PacketError::Capacity)?;
        Self::emit_validated(output, objects);
        Ok(length)
    }
    // 调用者已经检查完整对象长度与精确输出范围；两个发送入口共用一次字段写入，不重复遍历校验。
    fn emit_validated(output: &mut [u8], objects: &[Object<'_>]) {
        output[..4].copy_from_slice(&[0x20, 0, 0, 0]);
        let mut at = 4;
        for object in objects {
            let length = 4 + object.data.len();
            output[at..at + 2].copy_from_slice(&(length as u16).to_be_bytes());
            output[at + 2] = object.class;
            output[at + 3] = object.subtype;
            output[at + 4..at + length].copy_from_slice(object.data);
            at += length;
        }
        let value = checksum(output);
        // 正零的反码编码用负零表示，避免把实际传送的 checksum 误解为“未提供”。
        output[2..4].copy_from_slice(&if value == 0 { u16::MAX } else { value }.to_be_bytes());
    }
}

/// 标准四字节对象头与不透明 payload；payload 的协议含义由对应 Class/C-Type 解释者拥有。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Object<'a> {
    pub class: u8,
    pub subtype: u8,
    pub data: &'a [u8],
}
fn encoded_length(objects: &[Object<'_>]) -> Result<usize, PacketError> {
    if objects.is_empty() {
        return Err(PacketError::Malformed);
    }
    objects.iter().try_fold(4usize, |total, object| {
        let length = object
            .data
            .len()
            .checked_add(4)
            .ok_or(PacketError::Malformed)?;
        if !length.is_multiple_of(4) || u16::try_from(length).is_err() {
            return Err(PacketError::Malformed);
        }
        total.checked_add(length).ok_or(PacketError::Malformed)
    })
}

// 1. 两族先做全部尺寸/对象检查；2. 在最终消息复制引用并清 padding；3. 生成同一扩展正文。
// unit 只由两族固定布局提供，不接受线上任意对齐；这里不判断外层 IP MTU 或触发错误的策略。
pub(crate) fn emit_error_body(
    bytes: &mut [u8],
    quote: &[u8],
    unit: usize,
    objects: &[Object<'_>],
) -> Result<(usize, u8), PacketError> {
    let (padded, extensions, units) = if objects.is_empty() {
        (quote.len(), 0, 0)
    } else {
        let padded = quote
            .len()
            .max(128)
            .checked_next_multiple_of(unit)
            .ok_or(PacketError::Malformed)?;
        let units = u8::try_from(padded / unit).map_err(|_| PacketError::Malformed)?;
        (padded, encoded_length(objects)?, units)
    };
    let length = padded
        .checked_add(8)
        .and_then(|length| length.checked_add(extensions))
        .ok_or(PacketError::Malformed)?;
    u32::try_from(length).map_err(|_| PacketError::Malformed)?;
    let output = bytes.get_mut(..length).ok_or(PacketError::Capacity)?;
    output[8..8 + quote.len()].copy_from_slice(quote);
    output[8 + quote.len()..8 + padded].fill(0);
    if extensions != 0 {
        Extensions::emit_validated(&mut output[8 + padded..], objects);
    }
    Ok((length, units))
}
