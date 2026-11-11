//! 连接查询和管理协议，沿授权配置通道传递。连接身份在同一执行实例内单调递增。

use crate::{Decoder, Invalid};
use alloc::vec::Vec;
use core::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

pub mod changes;

const VERSION: u32 = 1;
const HEADER: usize = 20;
const ROW: usize = 56;
/// 单次回复最多包含的实际连接数。
pub const PAGE_LIMIT: usize = 64;
/// 最大分页正文长度；传输端在映射正文前校验。
pub const MAX_PAGE_BYTES: u64 = (HEADER + ROW * PAGE_LIMIT) as u64;
/// 固定分页请求长度。
pub const CURSOR_BYTES: usize = 20;

pub(crate) fn valid_page_length(length: u64) -> bool {
    (HEADER as u64..=MAX_PAGE_BYTES).contains(&length)
        && (length - HEADER as u64).is_multiple_of(ROW as u64)
}

/// 执行实例与上次检查到的连接 ID。全零开启一次新遍历。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Cursor {
    /// 转发实例标识；首次查询为零。
    pub epoch: u64,
    /// 上一页返回的继续位置；首次查询为零。
    pub after: u64,
}

impl Cursor {
    /// 编码一次分页请求。零实例仅用于起始游标。
    pub fn encode(self) -> [u8; CURSOR_BYTES] {
        let mut bytes = [0; CURSOR_BYTES];
        bytes[..4].copy_from_slice(&VERSION.to_le_bytes());
        bytes[4..12].copy_from_slice(&self.epoch.to_le_bytes());
        bytes[12..20].copy_from_slice(&self.after.to_le_bytes());
        bytes
    }

    /// 解码并验证游标。
    /// # Errors
    /// 长度、版本或起始游标的字段不合法。
    pub fn parse(bytes: &[u8]) -> Result<Self, Invalid> {
        let mut input = Decoder::new(bytes);
        if input.u32()? != VERSION {
            return Err(Invalid);
        }
        let cursor = Self {
            epoch: input.u64()?,
            after: input.u64()?,
        };
        input.finish()?;
        if (cursor.epoch == 0) != (cursor.after == 0) {
            return Err(Invalid);
        }
        Ok(cursor)
    }
}

/// 管理操作指定的单条连接。实例变化或同一端点复用时，旧身份不能命中新连接。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Identity {
    /// 转发实例标识。
    pub epoch: u64,
    /// 该实例内的连接 ID。
    pub id: u64,
}

impl Identity {
    /// 编码实例和连接 ID。两者必须非零才会被接收方接受。
    pub fn encode(self) -> [u8; CURSOR_BYTES] {
        Cursor {
            epoch: self.epoch,
            after: self.id,
        }
        .encode()
    }

    /// 解码固定长度的非零连接身份。
    /// # Errors
    /// 长度、版本或身份字段不合法。
    pub fn parse(bytes: &[u8]) -> Result<Self, Invalid> {
        let Cursor { epoch, after: id } = Cursor::parse(bytes)?;
        if id == 0 {
            return Err(Invalid);
        }
        Ok(Self { epoch, id })
    }
}

/// 连接所跟踪的传输协议。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum Protocol {
    Tcp = 1,
    Udp = 2,
    Echo = 3,
}

/// 当前连接状态。与逐包的 new/established 判定独立。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum Phase {
    Opening = 1,
    Established = 2,
    Closing = 3,
    Reset = 4,
}

/// 单条活动连接。Echo 用 source.port 携带标识符，destination.port 为零。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Connection {
    /// 在转发实例内单调分配的连接 ID。
    pub id: u64,
    /// 首次转发方向的源端点。
    pub source: SocketAddr,
    /// 首次转发方向的目的端点。
    pub destination: SocketAddr,
    /// 传输协议或 Echo。
    pub protocol: Protocol,
    /// 查询时的连接阶段。
    pub phase: Phase,
    /// 到空闲超时的剩余毫秒数。
    pub idle_ms: u64,
}

impl Connection {
    fn valid(&self) -> bool {
        self.source.is_ipv4() == self.destination.is_ipv4()
            && self.id != 0
            && [self.source, self.destination]
                .into_iter()
                .all(|endpoint| match endpoint {
                    SocketAddr::V4(_) => true,
                    SocketAddr::V6(endpoint) => {
                        endpoint.scope_id() == 0 && endpoint.flowinfo() == 0
                    }
                })
            && (if self.protocol == Protocol::Echo {
                self.destination.port() == 0
            } else {
                self.source.port() != 0 && self.destination.port() != 0
            })
            && (self.protocol == Protocol::Tcp
                || matches!(self.phase, Phase::Opening | Phase::Established))
    }

    fn write_to(&self, bytes: &mut Vec<u8>) {
        bytes.extend_from_slice(&self.id.to_le_bytes());
        bytes.extend_from_slice(&self.idle_ms.to_le_bytes());
        bytes.extend_from_slice(&[
            if self.source.is_ipv4() { 4 } else { 6 },
            self.protocol as u8,
            self.phase as u8,
            0,
        ]);
        for endpoint in [self.source, self.destination] {
            bytes.extend_from_slice(&endpoint.port().to_le_bytes());
            match endpoint.ip() {
                IpAddr::V4(address) => {
                    bytes.extend_from_slice(&address.octets());
                    bytes.extend_from_slice(&[0; 12]);
                }
                IpAddr::V6(address) => bytes.extend_from_slice(&address.octets()),
            }
        }
    }

    fn read(bytes: &[u8]) -> Result<Self, Invalid> {
        let mut input = Decoder::new(bytes);
        let id = input.u64()?;
        let idle_ms = input.u64()?;
        let family = input.u8()?;
        let protocol = match input.u8()? {
            1 => Protocol::Tcp,
            2 => Protocol::Udp,
            3 => Protocol::Echo,
            _ => return Err(Invalid),
        };
        let phase = match input.u8()? {
            1 => Phase::Opening,
            2 => Phase::Established,
            3 => Phase::Closing,
            4 => Phase::Reset,
            _ => return Err(Invalid),
        };
        if input.u8()? != 0 {
            return Err(Invalid);
        }
        let mut endpoint = || {
            let port = u16::from_le_bytes(input.take(2)?.try_into().map_err(|_| Invalid)?);
            let octets: [u8; 16] = input.take(16)?.try_into().map_err(|_| Invalid)?;
            let address = match family {
                4 if octets[4..] == [0; 12] => {
                    IpAddr::from(Ipv4Addr::from([octets[0], octets[1], octets[2], octets[3]]))
                }
                6 => IpAddr::from(Ipv6Addr::from(octets)),
                _ => return Err(Invalid),
            };
            Ok(SocketAddr::new(address, port))
        };
        let source = endpoint()?;
        let destination = endpoint()?;
        input.finish()?;
        Ok(Self {
            id,
            idle_ms,
            protocol,
            phase,
            source,
            destination,
        })
    }
}

/// 一次在同一网络域读取的实时页。next 指向最后检查过的位置。
#[derive(Debug, Eq, PartialEq)]
pub struct Page {
    /// 本页所属的转发实例。
    pub epoch: u64,
    /// 继续位置，`None` 表示已读到当前末尾。
    pub next: Option<u64>,
    /// 按连接 ID 递增排列的有效记录。
    pub rows: Vec<Connection>,
}

impl Page {
    fn valid(&self, after: u64) -> bool {
        self.epoch != 0
            && self.rows.len() <= PAGE_LIMIT
            && self.rows.first().is_none_or(|row| row.id > after)
            && self.rows.is_sorted_by(|a, b| a.id < b.id)
            && self
                .next
                .is_none_or(|id| id > after && self.rows.last().is_none_or(|row| id >= row.id))
            && self.rows.iter().all(Connection::valid)
    }

    /// 编码一页。失败时不返回部分正文。
    /// # Errors
    /// 页字段无效或内存不足。
    pub fn encode(&self) -> Result<Vec<u8>, Invalid> {
        if !self.valid(0) {
            return Err(Invalid);
        }
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(HEADER + self.rows.len() * ROW)
            .map_err(|_| Invalid)?;
        bytes.extend_from_slice(&VERSION.to_le_bytes());
        bytes.extend_from_slice(&self.epoch.to_le_bytes());
        bytes.extend_from_slice(&self.next.unwrap_or(0).to_le_bytes());
        for row in &self.rows {
            row.write_to(&mut bytes);
        }
        Ok(bytes)
    }

    /// 解析与游标相符的完整页。跨实例游标、顺序错误及截断都会拒绝。
    /// # Errors
    /// 版本、字段、游标、正文边界无效或内存不足。
    pub fn parse(bytes: &[u8], cursor: Cursor) -> Result<Self, Invalid> {
        let mut input = Decoder::new(bytes);
        if input.u32()? != VERSION {
            return Err(Invalid);
        }
        let epoch = input.u64()?;
        let next = input.u64()?;
        let (rows, remainder) = input.0.as_chunks::<ROW>();
        if !remainder.is_empty()
            || rows.len() > PAGE_LIMIT
            || cursor.epoch != 0 && cursor.epoch != epoch
        {
            return Err(Invalid);
        }
        let mut page = Self {
            epoch,
            next: (next != 0).then_some(next),
            rows: Vec::new(),
        };
        page.rows
            .try_reserve_exact(rows.len())
            .map_err(|_| Invalid)?;
        for row in rows {
            page.rows.push(Connection::read(row)?);
        }
        page.valid(cursor.after).then_some(page).ok_or(Invalid)
    }
}
