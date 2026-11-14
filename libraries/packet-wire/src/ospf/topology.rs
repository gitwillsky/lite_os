//! 两版 Router-LSA 和 Network-LSA，保留 SPF 所需的有向链路及网络成员关系。

use super::{Lsa, Version, options_word, read_u16, read_u32};
use crate::PacketError;
use core::net::Ipv4Addr;

/// 一条 v2 Router-LSA 链路。`link_id` 与 `link_data` 的含义由链路类型决定。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RouterLinkV2<'a> {
    pub link_id: u32,
    pub link_data: u32,
    pub kind: u8,
    pub cost: u16,
    /// 旧 TOS metric 行。v2 SPF 使用基础 cost，仍须按长度越过这些行。
    pub tos_metrics: &'a [[u8; 4]],
}

impl<'a> RouterLinkV2<'a> {
    fn take(bytes: &'a [u8]) -> Result<(Self, &'a [u8]), PacketError> {
        let (fixed, tail) = bytes.split_at_checked(12).ok_or(PacketError::Truncated)?;
        let (tos, rest) = tail
            .split_at_checked(usize::from(fixed[9]) * 4)
            .ok_or(PacketError::Truncated)?;
        Ok((
            Self {
                link_id: read_u32(fixed, 0),
                link_data: read_u32(fixed, 4),
                kind: fixed[8],
                cost: read_u16(fixed, 10),
                tos_metrics: tos.as_chunks::<4>().0,
            },
            rest,
        ))
    }
}

/// v2 Router-LSA 的本地标志和链路集合。
#[derive(Clone, Copy, Debug)]
pub struct RouterLsaV2<'a> {
    pub flags: u8,
    links: &'a [u8],
    count: usize,
}

impl<'a> RouterLsaV2<'a> {
    /// 读取 type 1 LSA，校验链路计数及每条链路的 TOS 区域长度。
    /// # Errors
    /// 版本/类型不匹配、记录截断或计数不符。
    pub fn parse(lsa: Lsa<'a>) -> Result<Self, PacketError> {
        lsa.require_type(Version::V2, 1)?;
        let (fixed, links) = lsa
            .body()
            .split_at_checked(4)
            .ok_or(PacketError::Truncated)?;
        let count = usize::from(read_u16(fixed, 2));
        let mut rest = links;
        for _ in 0..count {
            (_, rest) = RouterLinkV2::take(rest)?;
        }
        if !rest.is_empty() {
            return Err(PacketError::Malformed);
        }
        Ok(Self {
            flags: fixed[0],
            links,
            count,
        })
    }

    /// 遍历含基础 metric 和可选 TOS 行的链路。
    pub fn links(self) -> impl ExactSizeIterator<Item = RouterLinkV2<'a>> + 'a {
        let mut bytes = self.links;
        (0..self.count).map(move |_| {
            let (link, rest) = RouterLinkV2::take(bytes).expect("checked router links");
            bytes = rest;
            link
        })
    }

    /// 写入 Router-LSA 正文，返回正文大小。
    /// # Errors
    /// 计数超出协议字段宽度或空间不足。失败时不修改缓冲区。
    pub fn write(
        output: &mut [u8],
        flags: u8,
        links: &[RouterLinkV2<'_>],
    ) -> Result<usize, PacketError> {
        let count = u16::try_from(links.len()).map_err(|_| PacketError::Capacity)?;
        let mut size = 4usize;
        for link in links {
            let tos = u8::try_from(link.tos_metrics.len()).map_err(|_| PacketError::Malformed)?;
            size = size
                .checked_add(12 + usize::from(tos) * 4)
                .ok_or(PacketError::Capacity)?;
        }
        let bytes = output.get_mut(..size).ok_or(PacketError::Capacity)?;
        bytes[0] = flags;
        bytes[1] = 0;
        bytes[2..4].copy_from_slice(&count.to_be_bytes());
        let mut tail = &mut bytes[4..];
        for link in links {
            let (fixed, rest) = tail.split_at_mut(12);
            fixed[..4].copy_from_slice(&link.link_id.to_be_bytes());
            fixed[4..8].copy_from_slice(&link.link_data.to_be_bytes());
            fixed[8] = link.kind;
            fixed[9] = link.tos_metrics.len() as u8;
            fixed[10..12].copy_from_slice(&link.cost.to_be_bytes());
            let (tos, rest) = rest.split_at_mut(link.tos_metrics.len() * 4);
            tos.as_chunks_mut::<4>().0.copy_from_slice(link.tos_metrics);
            tail = rest;
        }
        Ok(size)
    }
}

/// v3 Router-LSA 的定宽链路。两端接口 ID 和邻居 Router ID 分别保留。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RouterLinkV3 {
    pub kind: u8,
    pub cost: u16,
    pub interface: u32,
    pub neighbor_interface: u32,
    pub neighbor_router: u32,
}

/// v3 Router-LSA 的标志、选项和链路集合。
#[derive(Clone, Copy, Debug)]
pub struct RouterLsaV3<'a> {
    pub flags: u8,
    pub options: u32,
    links: &'a [[u8; 16]],
}

impl<'a> RouterLsaV3<'a> {
    /// 读取区域作用域 Router-LSA 并校验定宽链路的边界。
    /// # Errors
    /// 类型错误、头截断或链路列表未对齐。
    pub fn parse(lsa: Lsa<'a>) -> Result<Self, PacketError> {
        lsa.require_type(Version::V3, 0x2001)?;
        let (fixed, body) = lsa
            .body()
            .split_at_checked(4)
            .ok_or(PacketError::Truncated)?;
        let (links, rest) = body.as_chunks::<16>();
        if !rest.is_empty() {
            return Err(PacketError::Malformed);
        }
        Ok(Self {
            flags: fixed[0],
            options: read_u32(fixed, 0) & 0x00ff_ffff,
            links,
        })
    }

    /// 遍历已验证边界的链路。
    pub fn links(self) -> impl ExactSizeIterator<Item = RouterLinkV3> + 'a {
        self.links.iter().map(|link| RouterLinkV3 {
            kind: link[0],
            cost: read_u16(link, 2),
            interface: read_u32(link, 4),
            neighbor_interface: read_u32(link, 8),
            neighbor_router: read_u32(link, 12),
        })
    }

    /// 写入 v3 Router-LSA 正文及链路，返回正文大小。
    /// # Errors
    /// 选项超过 24 位或空间不足。失败时不修改缓冲区。
    pub fn write(
        output: &mut [u8],
        flags: u8,
        options: u32,
        links: &[RouterLinkV3],
    ) -> Result<usize, PacketError> {
        let head = options_word(flags, options)?;
        let size = links
            .len()
            .checked_mul(16)
            .and_then(|size| size.checked_add(4))
            .ok_or(PacketError::Capacity)?;
        let bytes = output.get_mut(..size).ok_or(PacketError::Capacity)?;
        bytes[..4].copy_from_slice(&head);
        for (row, link) in bytes[4..].as_chunks_mut::<16>().0.iter_mut().zip(links) {
            row[0] = link.kind;
            row[1] = 0;
            row[2..4].copy_from_slice(&link.cost.to_be_bytes());
            row[4..8].copy_from_slice(&link.interface.to_be_bytes());
            row[8..12].copy_from_slice(&link.neighbor_interface.to_be_bytes());
            row[12..16].copy_from_slice(&link.neighbor_router.to_be_bytes());
        }
        Ok(size)
    }
}

/// Network-LSA 的版本专有字段。v3 前缀由另外的 LSA 发布。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NetworkParameters {
    V2 { mask: Ipv4Addr },
    V3 { options: u32 },
}

/// 指定广播网络上已与 DR 建立邻接的路由器。
#[derive(Clone, Copy, Debug)]
pub struct NetworkLsa<'a> {
    pub parameters: NetworkParameters,
    routers: &'a [[u8; 4]],
}

impl<'a> NetworkLsa<'a> {
    /// 读取 v2 type 2 或 v3 区域作用域 Network-LSA。
    /// # Errors
    /// 类型、头长度或 Router ID 行边界无效。
    pub fn parse(lsa: Lsa<'a>) -> Result<Self, PacketError> {
        let version = lsa.header().version();
        lsa.require_type(version, if version == Version::V2 { 2 } else { 0x2002 })?;
        let (head, body) = lsa
            .body()
            .split_at_checked(4)
            .ok_or(PacketError::Truncated)?;
        let (routers, rest) = body.as_chunks::<4>();
        if !rest.is_empty() {
            return Err(PacketError::Malformed);
        }
        let parameters = match version {
            Version::V2 => NetworkParameters::V2 {
                mask: Ipv4Addr::from(read_u32(head, 0)),
            },
            Version::V3 => NetworkParameters::V3 {
                options: read_u32(head, 0) & 0x00ff_ffff,
            },
        };
        Ok(Self {
            parameters,
            routers,
        })
    }

    /// 遍历 Network-LSA 所列 Router ID。
    pub fn routers(self) -> impl ExactSizeIterator<Item = u32> + 'a {
        self.routers.iter().copied().map(u32::from_be_bytes)
    }

    /// 写入 Network-LSA 正文，返回所用字节数。
    /// # Errors
    /// 选项超宽或空间不足。失败时不修改缓冲区。
    pub fn write(
        output: &mut [u8],
        parameters: NetworkParameters,
        routers: &[u32],
    ) -> Result<usize, PacketError> {
        let head = match parameters {
            NetworkParameters::V2 { mask } => mask.octets(),
            NetworkParameters::V3 { options } => options_word(0, options)?,
        };
        let size = routers
            .len()
            .checked_mul(4)
            .and_then(|size| size.checked_add(4))
            .ok_or(PacketError::Capacity)?;
        let bytes = output.get_mut(..size).ok_or(PacketError::Capacity)?;
        bytes[..4].copy_from_slice(&head);
        for (row, router) in bytes[4..].as_chunks_mut::<4>().0.iter_mut().zip(routers) {
            *row = router.to_be_bytes();
        }
        Ok(size)
    }
}
