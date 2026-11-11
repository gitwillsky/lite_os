//! BGP 的 TCP 流消息边界；协商、错误处理与连接状态不属于格式层。
use crate::PacketError;
pub mod attributes;
pub mod nlri;
pub mod open;
pub mod update;
use open::Open;
use update::Update;

/// BGP 共同头的固定字节数。
pub const HEADER_BYTES: usize = 19;
/// 未协商扩展消息的单帧上限；OPEN 始终使用此上限。
pub const STANDARD_LIMIT: usize = 4096;

/// 标准消息类型；未知类型由消息头 owner 明确拒绝。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    Open = 1,
    Update = 2,
    Notification = 3,
    Keepalive = 4,
    RouteRefresh = 5,
}
/// 消息头错误，足以让协议 owner 选择 RFC 4271 的对应 NOTIFICATION，而非重新解读头字节。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeaderError {
    /// 尚未收齐声明的消息；流读取入口以 None 表示等待，不将它当作坏消息。
    Truncated,
    /// Marker 不是十六个 0xff。
    Marker,
    /// Length 与类型、已授权大小或完整帧边界不符。
    Length,
    /// 未知消息类型。
    Type,
}
fn kind(value: u8) -> Result<Kind, HeaderError> {
    match value {
        1 => Ok(Kind::Open),
        2 => Ok(Kind::Update),
        3 => Ok(Kind::Notification),
        4 => Ok(Kind::Keepalive),
        5 => Ok(Kind::RouteRefresh),
        _ => Err(HeaderError::Type),
    }
}
fn valid_length(kind: Kind, length: usize, extended: bool) -> bool {
    let limit = if extended && kind != Kind::Open {
        usize::from(u16::MAX)
    } else {
        STANDARD_LIMIT
    };
    length <= limit
        && match kind {
            Kind::Open => length >= 29,
            Kind::Update => length >= 23,
            Kind::Notification => length >= 21,
            Kind::Keepalive => length == HEADER_BYTES,
            // RFC 7313 通知包含完整畸形帧。只要求能识别 AFI 与 subtype，长度规则归会话。
            Kind::RouteRefresh => length >= 22,
        }
}

/// 已验证的共同头；长度在正文缓冲分配前已受协商能力约束。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    kind: Kind,
    length: u16,
}
impl Header {
    /// 只读取前十九字节，允许传入尚未收齐正文的流缓冲。
    /// # Parameters
    /// extended 是本方向已经授权的扩展消息能力，不能因本地支持便无条件打开。
    /// # Errors
    /// 共同头截断、marker/类型/长度错误；OPEN/KEEPALIVE 不因扩展能力放宽。
    pub fn parse(bytes: &[u8], extended: bool) -> Result<Self, HeaderError> {
        let bytes = bytes.get(..HEADER_BYTES).ok_or(HeaderError::Truncated)?;
        if bytes[..16] != [0xff; 16] {
            return Err(HeaderError::Marker);
        }
        let kind = kind(bytes[18])?;
        let length = u16::from_be_bytes([bytes[16], bytes[17]]);
        if !valid_length(kind, usize::from(length), extended) {
            return Err(HeaderError::Length);
        }
        Ok(Self { kind, length })
    }
    /// 消息类型。
    pub fn kind(self) -> Kind {
        self.kind
    }
    /// 包含共同头的完整消息长度。
    pub fn length(self) -> usize {
        usize::from(self.length)
    }
}

/// 恰好一条完整 BGP 消息的借用；尚未验证 OPEN/UPDATE 的内部字段。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Frame<'a> {
    header: Header,
    bytes: &'a [u8],
}
impl<'a> Frame<'a> {
    /// 检查独立完整帧；拒绝尾部，避免把同流后续消息误当当前消息正文。
    /// # Errors
    /// 消息头非法、正文截断或额外字节。
    pub fn parse(bytes: &'a [u8], extended: bool) -> Result<Self, HeaderError> {
        let header = Header::parse(bytes, extended)?;
        if bytes.len() < header.length() {
            return Err(HeaderError::Truncated);
        }
        if bytes.len() != header.length() {
            return Err(HeaderError::Length);
        }
        Ok(Self { header, bytes })
    }
    /// 无复制取出流缓冲的首帧与未消费后缀；不完整返回 None，不扫描 marker 猜测重新同步。
    /// # Errors
    /// 头完整但非法时立即返回错误，不能按不可信长度扩展接收缓冲。
    pub fn read(bytes: &'a [u8], extended: bool) -> Result<Option<(Self, &'a [u8])>, HeaderError> {
        if bytes.len() < HEADER_BYTES {
            return Ok(None);
        }
        let header = Header::parse(bytes, extended)?;
        if bytes.len() < header.length() {
            return Ok(None);
        }
        let (bytes, rest) = bytes.split_at(header.length());
        Ok(Some((Self { header, bytes }, rest)))
    }
    /// 当前帧共同头。
    pub fn header(self) -> Header {
        self.header
    }
    /// 已验证头中的原始 Length 两字节，供 Bad Message Length 通知引用，不在协议状态层重建字段偏移。
    pub fn length_bytes(self) -> &'a [u8; 2] {
        self.bytes[16..18]
            .try_into()
            .expect("validated header length field")
    }
    /// 完整帧，不包含后续消息。
    pub fn bytes(self) -> &'a [u8] {
        self.bytes
    }
    /// 检查本消息的内部 framing；UPDATE 的属性/前缀可单独检查，便于 RFC 7606 分类处理。
    /// # Errors
    /// OPEN 参数 framing 或 UPDATE 两个长度区间非法。
    pub fn message(self) -> Result<Message<'a>, PacketError> {
        let body = &self.bytes[HEADER_BYTES..];
        Ok(match self.header.kind {
            Kind::Open => Message::Open(Open::parse(body)?),
            Kind::Update => Message::Update(Update::parse(body)?),
            Kind::Notification => Message::Notification {
                code: body[0],
                subcode: body[1],
                data: &body[2..],
            },
            Kind::Keepalive => Message::Keepalive,
            Kind::RouteRefresh => Message::RouteRefresh {
                afi: u16::from_be_bytes([body[0], body[1]]),
                subtype: body[2],
                safi: body.get(3).copied(),
            },
        })
    }
}

/// 消息内部的借用视图；未知通知 code/subcode 与刷新 subtype 保留给协议 owner。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Message<'a> {
    Open(Open<'a>),
    Update(Update<'a>),
    Notification {
        code: u8,
        subcode: u8,
        data: &'a [u8],
    },
    Keepalive,
    RouteRefresh {
        afi: u16,
        subtype: u8,
        /// None 表示可识别 subtype，但帧在 SAFI 之前结束。
        safi: Option<u8>,
    },
}

/// 构造单播刷新请求或 RFC 7313 起止标记，无 ORF 正文。
pub fn route_refresh(family: nlri::Family, subtype: u8) -> [u8; HEADER_BYTES + 4] {
    let mut bytes = [0; HEADER_BYTES + 4];
    let afi = match family {
        nlri::Family::Ipv4 => 1,
        nlri::Family::Ipv6 => 2,
    };
    bytes[HEADER_BYTES..].copy_from_slice(&[0, afi, subtype, 1]);
    emit(&mut bytes, Kind::RouteRefresh, false).expect("fixed route refresh length");
    bytes
}

/// 在正文已就位的完整输出范围内写共同头，不移动正文或修改额外容量。
/// # Errors
/// 类型所需固定字段不足、超出 u16/已授权上限；失败不改变输出。
pub fn emit(bytes: &mut [u8], kind: Kind, extended: bool) -> Result<(), PacketError> {
    if !valid_length(kind, bytes.len(), extended) {
        return Err(PacketError::Malformed);
    }
    let length = bytes.len() as u16;
    bytes[..16].fill(0xff);
    bytes[16..18].copy_from_slice(&length.to_be_bytes());
    bytes[18] = kind as u8;
    Ok(())
}

/// 在最终缓冲写 NOTIFICATION 的代码与原始错误数据，返回实际帧长。
/// # Errors
/// 总长溢出/超出方向已授权上限或容量不足；失败不修改输出，不截断必需错误数据。
pub fn emit_notification(
    bytes: &mut [u8],
    code: u8,
    subcode: u8,
    data: &[u8],
    extended: bool,
) -> Result<usize, PacketError> {
    let length = data
        .len()
        .checked_add(HEADER_BYTES + 2)
        .ok_or(PacketError::Malformed)?;
    if !valid_length(Kind::Notification, length, extended) {
        return Err(PacketError::Malformed);
    }
    let bytes = bytes.get_mut(..length).ok_or(PacketError::Capacity)?;
    bytes[19] = code;
    bytes[20] = subcode;
    bytes[21..].copy_from_slice(data);
    emit(bytes, Kind::Notification, extended).expect("notification size prevalidated");
    Ok(length)
}
