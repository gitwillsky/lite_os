//! 双栈邻居事实与管理分页。邻居状态仅由 ARP/ND owner 产生。
use crate::{Decoder, Invalid};
use alloc::vec::Vec;
use core::net::{IpAddr, Ipv4Addr, Ipv6Addr};

pub mod observation;

/// 原生快照及管理分页共用的固定邻居记录长度。
pub const ROW_BYTES: usize = 32;
/// 每页最大显示条数。
pub const LIMIT: usize = 64;
/// 版本、接口编号、地址族与固定宽度地址。
pub const CURSOR_BYTES: usize = 25;
const VERSION: u32 = 1;
const HEADER: usize = 6;
const ENTRY_BYTES: usize = ROW_BYTES + 16;

/// 有序分页键，同一地址在不同接口内各自独立。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Key {
    /// 逻辑接口编号。
    pub interface: u32,
    /// IPv4 或 IPv6 对端地址。
    pub address: IpAddr,
}

/// 编码要跳过的最后一条邻居。None 从第一个可见接口开始。
pub fn cursor(after: Option<Key>) -> [u8; CURSOR_BYTES] {
    let mut bytes = [0; CURSOR_BYTES];
    bytes[..4].copy_from_slice(&VERSION.to_le_bytes());
    if let Some(key) = after {
        bytes[4..8].copy_from_slice(&key.interface.to_le_bytes());
        match key.address {
            IpAddr::V4(address) => {
                bytes[8] = 4;
                bytes[9..13].copy_from_slice(&address.octets());
            }
            IpAddr::V6(address) => {
                bytes[8] = 6;
                bytes[9..].copy_from_slice(&address.octets());
            }
        }
    }
    bytes
}

/// 解码页起点。
/// # Errors
/// 版本、长度、接口或地址编码无效。
pub fn parse_cursor(bytes: &[u8]) -> Result<Option<Key>, Invalid> {
    let mut decoder = Decoder::new(bytes);
    if decoder.u32()? != VERSION {
        return Err(Invalid);
    }
    let interface = decoder.u32()?;
    let family = decoder.u8()?;
    let address: [u8; 16] = decoder.take(16)?.try_into().expect("address width");
    decoder.finish()?;
    let address = match (interface, family) {
        (0, 0) if address == [0; 16] => return Ok(None),
        (0, _) => return Err(Invalid),
        (_, 4) if address[4..] == [0; 12] => {
            IpAddr::from(<[u8; 4]>::try_from(&address[..4]).expect("IPv4 width"))
        }
        (_, 6) => IpAddr::from(address),
        _ => return Err(Invalid),
    };
    Ok(Some(Key { interface, address }))
}

/// 邻居事实及在同一次捕获中读取的系统接口名。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    /// ARP 或 ND 的实际记录。
    pub neighbor: Neighbor,
    /// 接口名称，以零补齐；空名称无效。
    pub interface_name: [u8; 16],
}

impl Entry {
    fn validate_name(&self) -> Result<(), Invalid> {
        let end = self
            .interface_name
            .iter()
            .position(|&byte| byte == 0)
            .unwrap_or(16);
        if end == 0
            || self.interface_name[..end].iter().any(u8::is_ascii_control)
            || self.interface_name[end..].iter().any(|&byte| byte != 0)
        {
            return Err(Invalid);
        }
        Ok(())
    }

    fn write(self, output: &mut Vec<u8>) -> Result<(), Invalid> {
        self.validate_name()?;
        output.extend_from_slice(&self.neighbor.encode()?);
        output.extend_from_slice(&self.interface_name);
        Ok(())
    }

    fn read(decoder: &mut Decoder<'_>) -> Result<Self, Invalid> {
        let entry = Self {
            neighbor: Neighbor::decode(decoder.take(ROW_BYTES)?)?,
            interface_name: decoder.take(16)?.try_into().expect("interface name width"),
        };
        entry.validate_name()?;
        Ok(entry)
    }
}

/// 从指定起点捕获的邻居页，按接口与地址升序排列。
#[derive(Debug, PartialEq, Eq)]
pub struct Page {
    /// 已捕获的邻居。
    pub rows: Vec<Entry>,
    /// 页尾后还有至少一条记录。
    pub more: bool,
}

/// 管理通道中邻居页正文的长度边界。
pub(crate) fn valid_page_length(length: u64) -> bool {
    (HEADER as u64..=(HEADER + LIMIT * ENTRY_BYTES) as u64).contains(&length)
        && (length - HEADER as u64).is_multiple_of(ENTRY_BYTES as u64)
}

impl Page {
    fn validate(&self, after: Option<Key>) -> Result<(), Invalid> {
        if self.rows.len() > LIMIT
            || self.more && self.rows.len() != LIMIT
            || self
                .rows
                .first()
                .is_some_and(|row| after.is_some_and(|key| row.neighbor.key() <= key))
            || !self
                .rows
                .is_sorted_by(|a, b| a.neighbor.key() < b.neighbor.key())
        {
            return Err(Invalid);
        }
        Ok(())
    }

    /// 编码当前页。
    /// # Errors
    /// 页边界或行内容无效，或分配失败。
    pub fn encode(&self) -> Result<Vec<u8>, Invalid> {
        self.validate(None)?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(HEADER + self.rows.len() * ENTRY_BYTES)
            .map_err(|_| Invalid)?;
        bytes.extend_from_slice(&VERSION.to_le_bytes());
        bytes.extend_from_slice(&[self.rows.len() as u8, u8::from(self.more)]);
        for row in &self.rows {
            row.write(&mut bytes)?;
        }
        Ok(bytes)
    }

    /// 解码页并核对续读键。不会返回部分页。
    /// # Errors
    /// 长度、字段、页序、游标无效或内存不足。
    pub fn parse(bytes: &[u8], after: Option<Key>) -> Result<Self, Invalid> {
        if !valid_page_length(bytes.len() as u64) {
            return Err(Invalid);
        }
        let mut decoder = Decoder::new(bytes);
        if decoder.u32()? != VERSION {
            return Err(Invalid);
        }
        let (count, more) = extent(&mut decoder)?;
        if bytes.len() != HEADER + count * ENTRY_BYTES {
            return Err(Invalid);
        }
        let mut rows = Vec::new();
        rows.try_reserve_exact(count).map_err(|_| Invalid)?;
        for _ in 0..count {
            rows.push(Entry::read(&mut decoder)?);
        }
        decoder.finish()?;
        let page = Self { rows, more };
        page.validate(after)?;
        Ok(page)
    }
}

fn extent(decoder: &mut Decoder<'_>) -> Result<(usize, bool), Invalid> {
    let count = usize::from(decoder.u8()?);
    let more = match decoder.u8()? {
        0 => false,
        1 if count == LIMIT => true,
        _ => return Err(Invalid),
    };
    if count > LIMIT {
        return Err(Invalid);
    }
    Ok((count, more))
}

/// 已观察到的原生邻居可达状态，包含为网关选路保留的探测失败事实。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NeighborState {
    Incomplete,
    Reachable,
    Stale,
    Delay,
    Probe,
    Failed,
}

/// 一条邻居事实；逻辑接口标识与本族地址共同确定身份。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Neighbor {
    /// 原生逻辑接口身份；非零 u32，不按应用 ABI 截断。
    pub interface_index: u32,
    /// 当前记录的对端地址，身份还需包含所在接口。
    pub address: IpAddr,
    /// 只由有效 ARP/ND 学习的单播 MAC；解析未完成时为 None。
    pub hardware_address: Option<[u8; 6]>,
    pub state: NeighborState,
    /// 已接受的 IPv6 Router 位。IPv4 始终为 false。
    pub is_router: bool,
    /// 当前 INCOMPLETE/PROBE 阶段已成功发出的探测次数；其他状态为零。
    pub probes: u8,
}
impl Neighbor {
    fn valid(self) -> bool {
        self.interface_index != 0
            && match self.address {
                IpAddr::V4(address) => {
                    !self.is_router && matches!(address.octets()[0], 1..=126 | 128..=223)
                }
                IpAddr::V6(address) => {
                    !address.is_unspecified()
                        && !address.is_multicast()
                        && !address.is_loopback()
                        && address.to_ipv4_mapped().is_none()
                }
            }
            && matches!(
                self.state,
                NeighborState::Incomplete | NeighborState::Failed
            ) == self.hardware_address.is_none()
            && self
                .hardware_address
                .is_none_or(|mac| mac != [0; 6] && mac[0] & 1 == 0)
            && (matches!(self.state, NeighborState::Incomplete | NeighborState::Probe)
                || self.probes == 0)
    }
}

impl Neighbor {
    /// 生成用于分页和比较的稳定键。
    pub fn key(self) -> Key {
        Key {
            interface: self.interface_index,
            address: self.address,
        }
    }

    /// 编码一条邻居事实。
    /// # Errors
    /// 地址、MAC 或状态不一致。
    pub fn encode(self) -> Result<[u8; ROW_BYTES], Invalid> {
        if !self.valid() {
            return Err(Invalid);
        }
        let mut output = [0; ROW_BYTES];
        output[..4].copy_from_slice(&self.interface_index.to_le_bytes());
        output[4] = match self.state {
            NeighborState::Incomplete => 0,
            NeighborState::Reachable => 1,
            NeighborState::Stale => 2,
            NeighborState::Delay => 3,
            NeighborState::Probe => 4,
            NeighborState::Failed => 5,
        };
        output[5] = u8::from(self.is_router);
        output[6] = self.probes;
        output[7] = u8::from(self.hardware_address.is_some());
        match self.address {
            IpAddr::V4(address) => {
                output[8..12].copy_from_slice(&address.octets());
                output[30] = 4;
            }
            IpAddr::V6(address) => {
                output[8..24].copy_from_slice(&address.octets());
                output[30] = 6;
            }
        }
        if let Some(mac) = self.hardware_address {
            output[24..30].copy_from_slice(&mac);
        }
        Ok(output)
    }
}

impl Neighbor {
    /// 解码一条已捕获的邻居事实。
    /// # Errors
    /// 长度、保留字段、地址或状态无效。
    pub fn decode(row: &[u8]) -> Result<Self, Invalid> {
        if row.len() != ROW_BYTES {
            return Err(Invalid);
        }
        let invalid = Invalid;
        let state = match row[4] {
            0 => NeighborState::Incomplete,
            1 => NeighborState::Reachable,
            2 => NeighborState::Stale,
            3 => NeighborState::Delay,
            4 => NeighborState::Probe,
            5 => NeighborState::Failed,
            _ => return Err(invalid),
        };
        if row[5] > 1 || row[7] > 1 || row[31] != 0 || row[7] == 0 && row[24..30] != [0; 6] {
            return Err(invalid);
        }
        let address = match row[30] {
            4 if row[12..24] == [0; 12] => IpAddr::V4(Ipv4Addr::from(
                <[u8; 4]>::try_from(&row[8..12]).expect("IPv4 width"),
            )),
            6 => IpAddr::V6(Ipv6Addr::from(
                <[u8; 16]>::try_from(&row[8..24]).expect("IPv6 width"),
            )),
            _ => return Err(invalid),
        };
        let result = Neighbor {
            interface_index: u32::from_le_bytes(row[..4].try_into().expect("row width")),
            address,
            hardware_address: (row[7] == 1).then(|| row[24..30].try_into().expect("MAC width")),
            state,
            is_router: row[5] == 1,
            probes: row[6],
        };
        if !result.valid() {
            return Err(invalid);
        }
        Ok(result)
    }
}
