//! 双栈 unicast/multicast 的前缀 NLRI。路径标识由会话方向能力决定；VPN/标签使用其他布局。
use crate::PacketError;
use core::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// NLRI 地址族，来源于所属 UPDATE/MP 属性，而非前缀字节自身。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Family {
    Ipv4,
    Ipv6,
}
impl Family {
    /// 地址族的最大前缀位数。
    pub fn bits(self) -> u8 {
        match self {
            Self::Ipv4 => 32,
            Self::Ipv6 => 128,
        }
    }
}
/// 已验证完整 NLRI 的借用；接收忽略前缀最后一字节的无关尾位。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Nlri<'a> {
    bytes: &'a [u8],
    family: Family,
    path_ids: bool,
}
/// 规范化前缀；发送也只写前缀有效位，不泄漏调用者地址中的主机位。
/// Eq/Ord 只比较结构身份，不表示最长匹配或路由优劣。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Prefix {
    pub address: IpAddr,
    pub length: u8,
}
impl<'a> Nlri<'a> {
    /// 一次检查全部前缀的长度与数据范围；空列表合法。
    /// # Errors
    /// 长度超出地址族宽度或前缀字节不足。
    pub fn parse(bytes: &'a [u8], family: Family) -> Result<Self, PacketError> {
        Self::parse_with_path_ids(bytes, family, false)
    }
    /// 按本方向协商的布局检查完整 NLRI；不能从字节猜测是否携带路径标识。
    /// # Errors
    /// 路径标识或前缀截断，或前缀长度超出地址族宽度。
    pub fn parse_with_path_ids(
        bytes: &'a [u8],
        family: Family,
        path_ids: bool,
    ) -> Result<Self, PacketError> {
        let mut rest = bytes;
        while !rest.is_empty() {
            let (length, tail) = rest
                .get(usize::from(path_ids) * 4..)
                .and_then(<[u8]>::split_first)
                .ok_or(PacketError::Truncated)?;
            let length = *length;
            if length > family.bits() {
                return Err(PacketError::Malformed);
            }
            rest = tail
                .get(usize::from(length).div_ceil(8)..)
                .ok_or(PacketError::Truncated)?;
        }
        Ok(Self {
            bytes,
            family,
            path_ids,
        })
    }
    /// 所属地址族。
    pub fn family(self) -> Family {
        self.family
    }
    /// 原始完整编码，保留接收时无关尾位。
    pub fn bytes(self) -> &'a [u8] {
        self.bytes
    }
    /// 按原顺序读取规范化前缀，不分配数组。
    pub fn prefixes(self) -> impl Iterator<Item = Prefix> {
        self.paths().map(|(prefix, _)| prefix)
    }
    /// 按原顺序读取前缀及方向协商的路径标识；标识 0 与无标识不同。
    pub fn paths(self) -> impl Iterator<Item = (Prefix, Option<u32>)> {
        let mut rest = self.bytes;
        core::iter::from_fn(move || {
            if rest.is_empty() {
                return None;
            }
            let path_id = self.path_ids.then(|| {
                let (id, tail) = rest.split_at(4);
                rest = tail;
                u32::from_be_bytes(id.try_into().expect("validated path ID"))
            });
            let (&length, tail) = rest.split_first()?;
            let count = usize::from(length).div_ceil(8);
            let mut address = [0; 16];
            address[..count].copy_from_slice(&tail[..count]);
            if length % 8 != 0 {
                address[count - 1] &= 0xff << (8 - length % 8);
            }
            rest = &tail[count..];
            Some((
                Prefix {
                    address: match self.family {
                        Family::Ipv4 => IpAddr::V4(Ipv4Addr::from(
                            <[u8; 4]>::try_from(&address[..4]).expect("fixed IPv4 width"),
                        )),
                        Family::Ipv6 => IpAddr::V6(Ipv6Addr::from(address)),
                    },
                    length,
                },
                path_id,
            ))
        })
    }
    /// 向调用者最终缓冲编码一个地址族的完整前缀列表，返回实际字节数。
    /// # Errors
    /// 地址族/长度非法、容量不足或长度溢出；失败不修改输出。
    pub fn emit(
        bytes: &mut [u8],
        family: Family,
        prefixes: &[Prefix],
    ) -> Result<usize, PacketError> {
        Self::emit_paths(
            bytes,
            family,
            prefixes.iter().copied().map(|prefix| (prefix, None)),
        )
    }
    /// 编码一个普通或全部携带路径标识的 NLRI 列表。
    /// # Errors
    /// 混合布局、族/长度非法、长度溢出或容量不足。失败不修改输出。
    pub fn emit_paths(
        bytes: &mut [u8],
        family: Family,
        paths: impl Iterator<Item = (Prefix, Option<u32>)> + Clone,
    ) -> Result<usize, PacketError> {
        let add_path = paths.clone().next().is_some_and(|(_, id)| id.is_some());
        let head = 1 + usize::from(add_path) * 4;
        let length = paths.clone().try_fold(0usize, |total, (prefix, id)| {
            if prefix.length > family.bits()
                || id.is_some() != add_path
                || !matches!(
                    (family, prefix.address),
                    (Family::Ipv4, IpAddr::V4(_)) | (Family::Ipv6, IpAddr::V6(_))
                )
            {
                return Err(PacketError::Malformed);
            }
            total
                .checked_add(head + usize::from(prefix.length).div_ceil(8))
                .ok_or(PacketError::Malformed)
        })?;
        let bytes = bytes.get_mut(..length).ok_or(PacketError::Capacity)?;
        let mut at = 0;
        for (prefix, id) in paths {
            if let Some(id) = id {
                bytes[at..at + 4].copy_from_slice(&id.to_be_bytes());
                at += 4;
            }
            bytes[at] = prefix.length;
            let count = usize::from(prefix.length).div_ceil(8);
            match prefix.address {
                IpAddr::V4(address) => {
                    bytes[at + 1..at + 1 + count].copy_from_slice(&address.octets()[..count])
                }
                IpAddr::V6(address) => {
                    bytes[at + 1..at + 1 + count].copy_from_slice(&address.octets()[..count])
                }
            }
            if prefix.length % 8 != 0 {
                bytes[at + count] &= 0xff << (8 - prefix.length % 8);
            }
            at += 1 + count;
        }
        Ok(length)
    }
}
