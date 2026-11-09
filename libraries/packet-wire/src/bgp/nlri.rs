//! 双栈 unicast/multicast 的普通前缀 NLRI；VPN/带标签/ADD-PATH 不能冒充此布局。
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
        let mut rest = bytes;
        while let Some((&length, tail)) = rest.split_first() {
            if length > family.bits() {
                return Err(PacketError::Malformed);
            }
            rest = tail
                .get(usize::from(length).div_ceil(8)..)
                .ok_or(PacketError::Truncated)?;
        }
        Ok(Self { bytes, family })
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
        let mut rest = self.bytes;
        core::iter::from_fn(move || {
            let (&length, tail) = rest.split_first()?;
            let count = usize::from(length).div_ceil(8);
            let mut address = [0; 16];
            address[..count].copy_from_slice(&tail[..count]);
            if length % 8 != 0 {
                address[count - 1] &= 0xff << (8 - length % 8);
            }
            rest = &tail[count..];
            Some(Prefix {
                address: match self.family {
                    Family::Ipv4 => IpAddr::V4(Ipv4Addr::from(
                        <[u8; 4]>::try_from(&address[..4]).expect("fixed IPv4 width"),
                    )),
                    Family::Ipv6 => IpAddr::V6(Ipv6Addr::from(address)),
                },
                length,
            })
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
        let length = prefixes.iter().try_fold(0usize, |total, prefix| {
            if prefix.length > family.bits()
                || !matches!(
                    (family, prefix.address),
                    (Family::Ipv4, IpAddr::V4(_)) | (Family::Ipv6, IpAddr::V6(_))
                )
            {
                return Err(PacketError::Malformed);
            }
            total
                .checked_add(1 + usize::from(prefix.length).div_ceil(8))
                .ok_or(PacketError::Malformed)
        })?;
        let bytes = bytes.get_mut(..length).ok_or(PacketError::Capacity)?;
        let mut at = 0;
        for prefix in prefixes {
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
