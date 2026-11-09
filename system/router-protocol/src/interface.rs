//! 接口来源计划及其执行投影：独立于 Linux ioctl 的批量、版本化描述。
//!
//! 当前执行服务只提供单 IPv4 地址；这是执行能力范围，不是最终配置树的 schema。
//! 批次先验证全部字节，再借用读取；资源预备、冲突与发布由执行服务唯一 owner 决定。

use core::net::Ipv4Addr;

use crate::Invalid;

/// 接口执行协议的版本；布局变化必须提升版本，两端不维护兼容分支。
pub const VERSION: u32 = 3;
const HEADER_BYTES: usize = 8;
const ENTRY_BYTES: usize = 28;

/// 接口配置 owner 提供的完整默认路由意图；执行侧不能自行生成此来源。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DefaultRoute {
    /// 立即 IPv4 网关，可达性由同一候选事实验证。
    pub gateway: Ipv4Addr,
    /// 用于观测的协议标记，不携带权限。
    pub protocol: u8,
    /// 由实际配置 owner 决定的偏好和来源内代价。
    pub attributes: crate::routing::Attributes,
}

/// 一个接口的完整 IPv4 计划；RIB 拆出默认路由来源后，向执行服务只传接口事实。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ipv4InterfaceConfiguration {
    /// 执行服务发布的非零接口身份，不是持久配置对象的名称。
    pub index: u32,
    /// 管理状态。
    pub up: bool,
    /// 要应用的 L3 MTU；None 保持执行设备当前值，供 DHCP 续租复用原接口策略。
    pub mtu: Option<u32>,
    /// `None` 撤销地址；有地址时 `prefix_length` 可为 0–32。
    pub address: Option<Ipv4Addr>,
    /// 地址的前缀长度；没有地址时必须为零。
    pub prefix_length: u8,
    /// 此接口的完整默认路由意图；RIB 捕获为来源，纯接口执行投影必须为 None。
    pub route: Option<DefaultRoute>,
}

impl Ipv4InterfaceConfiguration {
    /// 校验可表达性；不替执行服务判断网关是否可达或接口是否存在。
    ///
    /// # Errors
    /// 零身份、非法前缀/地址或缺少网关所需的已启用地址。
    pub fn validate(self) -> Result<(), Invalid> {
        if self.index == 0
            || self.prefix_length > 32
            || self.address.is_none() && self.prefix_length != 0
            || self.address.is_some_and(|address| {
                address.is_unspecified() || address.is_multicast() || address.is_broadcast()
            })
            || self.route.is_some_and(|route| {
                !self.up
                    || self.address.is_none()
                    || route.protocol == 0
                    || route.attributes.preference == 0
                    || route.gateway.is_unspecified()
                    || route.gateway.is_multicast()
                    || route.gateway.is_broadcast()
            })
        {
            return Err(Invalid);
        }
        Ok(())
    }
}

/// 已完整校验的配置批次，只借用调用者的字节；Default 表示复合事务内没有接口变更，不生成空独立 wire 批次。
#[derive(Clone, Copy, Default)]
pub struct InterfaceConfigurationBatch<'a>(&'a [u8]);

impl<'a> InterfaceConfigurationBatch<'a> {
    /// 返回编码 `count` 项所需的精确字节数。
    ///
    /// # Errors
    /// 空批次、数量超出 u32 或长度溢出。
    pub fn encoded_size(count: usize) -> Result<usize, Invalid> {
        if count == 0 || u32::try_from(count).is_err() {
            return Err(Invalid);
        }
        count
            .checked_mul(ENTRY_BYTES)
            .and_then(|size| size.checked_add(HEADER_BYTES))
            .ok_or(Invalid)
    }

    /// 在调用者存储中编码，返回实际长度；所有校验在写入前完成。
    ///
    /// # Errors
    /// 条目非法、批次大小非法或输出空间不足；错误时不改写输出。
    pub fn encode(
        entries: &[Ipv4InterfaceConfiguration],
        output: &mut [u8],
    ) -> Result<usize, Invalid> {
        let length = Self::encoded_size(entries.len())?;
        if output.len() < length {
            return Err(Invalid);
        }
        for entry in entries {
            entry.validate()?;
        }
        output[..4].copy_from_slice(&VERSION.to_le_bytes());
        output[4..8].copy_from_slice(&(entries.len() as u32).to_le_bytes());
        for (entry, bytes) in entries.iter().zip(
            output[HEADER_BYTES..length]
                .as_chunks_mut::<ENTRY_BYTES>()
                .0,
        ) {
            bytes[..4].copy_from_slice(&entry.index.to_le_bytes());
            bytes[4] = u8::from(entry.up)
                | (u8::from(entry.address.is_some()) << 1)
                | (u8::from(entry.route.is_some()) << 2)
                | (u8::from(entry.mtu.is_some()) << 3);
            bytes[5] = entry.prefix_length;
            bytes[6] = entry.route.map_or(0, |route| route.protocol);
            bytes[7] = 0;
            bytes[8..12].copy_from_slice(&entry.address.unwrap_or(Ipv4Addr::UNSPECIFIED).octets());
            bytes[12..16].copy_from_slice(
                &entry
                    .route
                    .map_or(Ipv4Addr::UNSPECIFIED, |route| route.gateway)
                    .octets(),
            );
            bytes[16..20].copy_from_slice(&entry.mtu.unwrap_or(0).to_le_bytes());
            bytes[20..24].copy_from_slice(
                &entry
                    .route
                    .map_or(0, |route| route.attributes.preference)
                    .to_le_bytes(),
            );
            bytes[24..28].copy_from_slice(
                &entry
                    .route
                    .map_or(0, |route| route.attributes.metric)
                    .to_le_bytes(),
            );
        }
        Ok(length)
    }

    /// 验证完整批次。重复接口身份属于执行事务的冲突检查，不属于字节解码。
    ///
    /// # Errors
    /// 版本、长度、保留位、未置位的非零字段或条目不合法。
    pub fn parse(bytes: &'a [u8]) -> Result<Self, Invalid> {
        let header = bytes.get(..HEADER_BYTES).ok_or(Invalid)?;
        if u32_at(header, 0) != VERSION
            || bytes.len() != Self::encoded_size(u32_at(header, 4) as usize)?
        {
            return Err(Invalid);
        }
        for entry in bytes[HEADER_BYTES..].as_chunks::<ENTRY_BYTES>().0 {
            if entry[4] & !15 != 0
                || entry[7] != 0
                || entry[4] & 2 == 0 && entry[8..12] != [0; 4]
                || entry[4] & 4 == 0
                    && (entry[6] != 0 || entry[12..16] != [0; 4] || entry[20..28] != [0; 8])
                || entry[4] & 8 == 0 && entry[16..20] != [0; 4]
            {
                return Err(Invalid);
            }
            decode(entry).validate()?;
        }
        Ok(Self(&bytes[HEADER_BYTES..]))
    }

    /// 逐项读取已验证字段，不重新分配或校验；返回值数量与批次完全一致。
    pub fn iter(self) -> impl ExactSizeIterator<Item = Ipv4InterfaceConfiguration> + 'a {
        self.0.as_chunks::<ENTRY_BYTES>().0.iter().map(decode)
    }
}

fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

fn decode(bytes: &[u8; ENTRY_BYTES]) -> Ipv4InterfaceConfiguration {
    Ipv4InterfaceConfiguration {
        index: u32_at(bytes, 0),
        up: bytes[4] & 1 != 0,
        mtu: (bytes[4] & 8 != 0).then(|| u32_at(bytes, 16)),
        address: (bytes[4] & 2 != 0).then(|| Ipv4Addr::from(u32_at(bytes, 8).to_le_bytes())),
        prefix_length: bytes[5],
        route: (bytes[4] & 4 != 0).then(|| DefaultRoute {
            gateway: Ipv4Addr::from(u32_at(bytes, 12).to_le_bytes()),
            protocol: bytes[6],
            attributes: crate::routing::Attributes {
                preference: u32_at(bytes, 20),
                metric: u32_at(bytes, 24),
            },
        }),
    }
}
