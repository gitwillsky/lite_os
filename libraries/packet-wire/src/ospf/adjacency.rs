//! Hello 及 Database Description 的版本专有字段和定宽行。

use super::{LsaHeader, LsaSummaries, Version, options_word, read_u16, read_u32};
use crate::PacketError;
use core::net::Ipv4Addr;

/// Hello 的链路标识。v2 携带网络掩码，v3 携带发送接口 ID。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HelloLink {
    V2(Ipv4Addr),
    V3(u32),
}

/// Hello 的定宽字段。v3 选项为 24 位，死亡间隔为 16 位。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HelloParameters {
    pub link: HelloLink,
    pub options: u32,
    pub priority: u8,
    pub hello_interval: u16,
    pub dead_interval: u32,
    pub designated_router: u32,
    pub backup_router: u32,
}

/// 已分出 Router ID 行的 Hello。路由器 ID 和间隔的接受条件由接口状态机判断。
#[derive(Clone, Copy, Debug)]
pub struct Hello<'a> {
    pub parameters: HelloParameters,
    neighbors: &'a [[u8; 4]],
}

impl<'a> Hello<'a> {
    pub(super) fn parse(version: Version, bytes: &'a [u8]) -> Result<Self, PacketError> {
        let (fixed, tail) = bytes.split_at_checked(20).ok_or(PacketError::Truncated)?;
        let (neighbors, remainder) = tail.as_chunks::<4>();
        if !remainder.is_empty() {
            return Err(PacketError::Malformed);
        }
        let (link, options, priority, hello_interval, dead_interval) = match version {
            Version::V2 => (
                HelloLink::V2(Ipv4Addr::from(read_u32(fixed, 0))),
                u32::from(fixed[6]),
                fixed[7],
                read_u16(fixed, 4),
                read_u32(fixed, 8),
            ),
            Version::V3 => (
                HelloLink::V3(read_u32(fixed, 0)),
                read_u32(fixed, 4) & 0x00ff_ffff,
                fixed[4],
                read_u16(fixed, 8),
                u32::from(read_u16(fixed, 10)),
            ),
        };
        let parameters = HelloParameters {
            link,
            options,
            priority,
            hello_interval,
            dead_interval,
            designated_router: read_u32(fixed, 12),
            backup_router: read_u32(fixed, 16),
        };
        Ok(Self {
            parameters,
            neighbors,
        })
    }

    /// 按线序遍历 Hello 中列出的邻居 Router ID。
    pub fn neighbors(self) -> impl ExactSizeIterator<Item = u32> + 'a {
        self.neighbors.iter().copied().map(u32::from_be_bytes)
    }

    /// 将 Hello 正文写入调用者的输出，返回正文长度。
    /// # Errors
    /// 字段超出所选版本的宽度或输出空间不足。失败时保持缓冲区原样。
    pub fn write(
        output: &mut [u8],
        hello: HelloParameters,
        neighbors: &[u32],
    ) -> Result<usize, PacketError> {
        let length = neighbors
            .len()
            .checked_mul(4)
            .and_then(|size| size.checked_add(20))
            .ok_or(PacketError::Capacity)?;
        let (fixed, rows) = output
            .get_mut(..length)
            .ok_or(PacketError::Capacity)?
            .split_at_mut(20);
        match hello.link {
            HelloLink::V2(mask) => {
                let options = u8::try_from(hello.options).map_err(|_| PacketError::Malformed)?;
                fixed[..4].copy_from_slice(&mask.octets());
                fixed[4..6].copy_from_slice(&hello.hello_interval.to_be_bytes());
                fixed[6] = options;
                fixed[7] = hello.priority;
                fixed[8..12].copy_from_slice(&hello.dead_interval.to_be_bytes());
            }
            HelloLink::V3(interface) => {
                let options = options_word(hello.priority, hello.options)?;
                let dead =
                    u16::try_from(hello.dead_interval).map_err(|_| PacketError::Malformed)?;
                fixed[..4].copy_from_slice(&interface.to_be_bytes());
                fixed[4..8].copy_from_slice(&options);
                fixed[8..10].copy_from_slice(&hello.hello_interval.to_be_bytes());
                fixed[10..12].copy_from_slice(&dead.to_be_bytes());
            }
        }
        fixed[12..16].copy_from_slice(&hello.designated_router.to_be_bytes());
        fixed[16..20].copy_from_slice(&hello.backup_router.to_be_bytes());
        for (row, router) in rows.as_chunks_mut::<4>().0.iter_mut().zip(neighbors) {
            row.copy_from_slice(&router.to_be_bytes());
        }
        Ok(length)
    }
}

/// DB Description 的初始化、更多和主端标志。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DatabaseFlags {
    pub init: bool,
    pub more: bool,
    pub master: bool,
}

impl DatabaseFlags {
    fn parse(bits: u8) -> Self {
        Self {
            init: bits & 4 != 0,
            more: bits & 2 != 0,
            master: bits & 1 != 0,
        }
    }

    fn bits(self) -> u8 {
        (u8::from(self.init) << 2) | (u8::from(self.more) << 1) | u8::from(self.master)
    }
}

/// DB Description 的定宽字段，不包含 LSA 正文。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DatabaseParameters {
    pub mtu: u16,
    pub options: u32,
    pub flags: DatabaseFlags,
    pub sequence: u32,
}

/// 已分出 20 字节 LSA 摘要的数据库描述报文。
#[derive(Clone, Copy, Debug)]
pub struct DatabaseDescription<'a> {
    pub parameters: DatabaseParameters,
    pub summaries: LsaSummaries<'a>,
}

impl<'a> DatabaseDescription<'a> {
    const fn fixed_len(version: Version) -> usize {
        match version {
            Version::V2 => 8,
            Version::V3 => 12,
        }
    }

    pub(super) fn parse(version: Version, bytes: &'a [u8]) -> Result<Self, PacketError> {
        let (fixed, tail) = bytes
            .split_at_checked(Self::fixed_len(version))
            .ok_or(PacketError::Truncated)?;
        let (mtu, options, flags, sequence) = match version {
            Version::V2 => (
                read_u16(fixed, 0),
                u32::from(fixed[2]),
                fixed[3],
                read_u32(fixed, 4),
            ),
            Version::V3 => (
                read_u16(fixed, 4),
                read_u32(fixed, 0) & 0x00ff_ffff,
                fixed[7],
                read_u32(fixed, 8),
            ),
        };
        let parameters = DatabaseParameters {
            mtu,
            options,
            flags: DatabaseFlags::parse(flags),
            sequence,
        };
        let summaries = LsaSummaries::parse(version, tail)?;
        Ok(Self {
            parameters,
            summaries,
        })
    }

    /// 写入数据库描述正文及摘要，返回所用字节数。
    /// # Errors
    /// 版本宽度不符、摘要版本不同或输出空间不足。失败时保持缓冲区原样。
    pub fn write(
        output: &mut [u8],
        version: Version,
        parameters: DatabaseParameters,
        summaries: &[LsaHeader<'_>],
    ) -> Result<usize, PacketError> {
        let head = Self::fixed_len(version);
        let options = options_word(0, parameters.options)?;
        if version == Version::V2 && parameters.options > u32::from(u8::MAX) {
            return Err(PacketError::Malformed);
        }
        let (fixed, tail) = output
            .split_at_mut_checked(head)
            .ok_or(PacketError::Capacity)?;
        let written = LsaSummaries::write(tail, version, summaries)?;
        match version {
            Version::V2 => {
                fixed[..2].copy_from_slice(&parameters.mtu.to_be_bytes());
                fixed[2] = options[3];
            }
            Version::V3 => {
                fixed[..4].copy_from_slice(&options);
                fixed[4..6].copy_from_slice(&parameters.mtu.to_be_bytes());
                fixed[6] = 0;
            }
        }
        fixed[head - 5] = parameters.flags.bits();
        fixed[head - 4..].copy_from_slice(&parameters.sequence.to_be_bytes());
        Ok(head + written)
    }
}
