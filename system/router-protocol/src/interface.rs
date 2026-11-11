//! 接口来源计划及其执行投影：独立于 Linux ioctl 的批量、版本化描述。
//!
//! 同一接口事务包含 IPv4 事实及可选 IPv6 地址替换；缺席保留，显式空集合清除手动地址。
//! 批次先验证全部字节，再借用读取；资源预备、冲突与发布由执行服务唯一 owner 决定。

use core::net::{Ipv4Addr, Ipv6Addr};

use crate::{
    Invalid,
    configuration::{
        AdvertisedDns, MAX_ADVERTISED_DNS, RouterAdvertisementPolicy, RouterAdvertisements,
    },
};
mod delegated;
mod ipv6;
pub use delegated::DelegatedPrefixes;
pub use ipv6::Ipv6Addresses;

/// 接口执行协议的版本；布局变化必须提升版本，两端不维护兼容分支。
pub const VERSION: u32 = 8;
/// 每个原生网络域保留给回环接口的逻辑身份。
pub const LOOPBACK_INDEX: u32 = 1;
const HEADER_BYTES: usize = 8;
const ENTRY_BYTES: usize = 44;

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

/// 一个接口的 IPv4 计划及可选 IPv6 地址替换；RIB 拆出默认路由来源后只传接口事实。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InterfaceConfiguration<'a> {
    /// 执行服务发布的非零接口身份，不是持久配置对象的名称。
    pub index: u32,
    /// None 保留 IPv6，Some 替换手动地址；视图只在预备阶段借用，不能逃逸为运行时状态。
    pub ipv6: Option<Ipv6Addresses<'a>>,
    /// None 保留，Some 以有期限的 /64 前缀替换当前下游委派。
    pub delegated: Option<DelegatedPrefixes<'a>>,
    /// 接口通告策略。None 保留当前值，独立于手动 IPv6 地址更新。
    pub accept_ra: Option<RouterAdvertisementPolicy>,
    /// None 保持下游通告，Some 设定或停止通告。
    pub advertise: Option<RouterAdvertisements>,
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

impl InterfaceConfiguration<'_> {
    /// 校验可表达性；不替执行服务判断网关是否可达或接口是否存在。
    ///
    /// # Errors
    /// 零身份、非法前缀/地址或缺少网关所需的已启用地址。
    pub fn validate(self) -> Result<(), Invalid> {
        if let Some(addresses) = self.ipv6 {
            addresses.validate()?;
        }
        if let Some(prefixes) = self.delegated {
            prefixes.validate()?;
        }
        if self.index == 0
            || self.advertise.is_some_and(|policy| !policy.valid())
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

/// 整批验证后的借用输入；资源与逆向状态由执行 owner 准备，不在 codec 中分配地址数组。
#[derive(Clone, Copy, Default)]
pub struct InterfaceConfigurationBatch<'a> {
    body: &'a [u8],
    count: usize,
}
impl<'a> InterfaceConfigurationBatch<'a> {
    /// 返回完整批次的精确尺寸，含可变 IPv6 地址正文。
    /// # Errors
    /// 空批次、项数或长度溢出。
    pub fn encoded_size(entries: &[InterfaceConfiguration<'_>]) -> Result<usize, Invalid> {
        if entries.is_empty() || u32::try_from(entries.len()).is_err() {
            return Err(Invalid);
        }
        entries.iter().try_fold(HEADER_BYTES, |size, entry| {
            let count = entry.ipv6.map_or(0, Ipv6Addresses::len);
            u32::try_from(count).map_err(|_| Invalid)?;
            count
                .checked_mul(ipv6::ROW)
                .and_then(|n| {
                    let delegated = entry.delegated.map_or(0, DelegatedPrefixes::len);
                    n.checked_add(delegated.checked_mul(delegated::ROW)?)
                })
                .and_then(|n| {
                    n.checked_add(
                        entry
                            .advertise
                            .map_or(0, |policy| policy.dns().as_slice().len())
                            * 16,
                    )
                })
                .and_then(|n| n.checked_add(ENTRY_BYTES))
                .and_then(|n| n.checked_add(size))
                .ok_or(Invalid)
        })
    }
    /// 先完整检查，随后直接写最终存储；不改变输出中的失败前像。
    /// # Errors
    /// 非法意图、长度或容量不足。
    pub fn encode(
        entries: &[InterfaceConfiguration<'_>],
        output: &mut [u8],
    ) -> Result<usize, Invalid> {
        let size = Self::encoded_size(entries)?;
        if output.len() < size {
            return Err(Invalid);
        }
        for entry in entries {
            entry.validate()?;
        }
        output[..4].copy_from_slice(&VERSION.to_le_bytes());
        output[4..8].copy_from_slice(&(entries.len() as u32).to_le_bytes());
        let mut offset = HEADER_BYTES;
        for entry in entries {
            let count = entry.ipv6.map_or(0, Ipv6Addresses::len);
            let delegated_count = entry.delegated.map_or(0, DelegatedPrefixes::len);
            let delegated_offset = ENTRY_BYTES + count * ipv6::ROW;
            let dns = entry
                .advertise
                .map_or_else(AdvertisedDns::default, RouterAdvertisements::dns);
            let dns_offset = delegated_offset + delegated_count * delegated::ROW;
            let end = offset + dns_offset + dns.as_slice().len() * 16;
            let bytes = &mut output[offset..end];
            bytes[..4].copy_from_slice(&entry.index.to_le_bytes());
            bytes[4] = u8::from(entry.up)
                | (u8::from(entry.address.is_some()) << 1)
                | (u8::from(entry.route.is_some()) << 2)
                | (u8::from(entry.mtu.is_some()) << 3)
                | (u8::from(entry.ipv6.is_some()) << 4)
                | (u8::from(entry.advertise.is_some()) << 5)
                | (u8::from(entry.delegated.is_some()) << 6);
            bytes[5] = entry.prefix_length;
            bytes[6] = entry.route.map_or(0, |route| route.protocol);
            bytes[7] = match entry.accept_ra {
                None => 0,
                Some(RouterAdvertisementPolicy::HostOnly) => 1,
                Some(RouterAdvertisementPolicy::Disabled) => 2,
                Some(RouterAdvertisementPolicy::Always) => 3,
            };
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
            bytes[28..32].copy_from_slice(&(count as u32).to_le_bytes());
            bytes[32..36].fill(0);
            if let Some(RouterAdvertisements::Enabled {
                max_interval_seconds,
                router_lifetime_seconds,
                ..
            }) = entry.advertise
            {
                bytes[32..34].copy_from_slice(&max_interval_seconds.to_le_bytes());
                bytes[34..36].copy_from_slice(&router_lifetime_seconds.to_le_bytes());
            }
            bytes[36..40].copy_from_slice(&(delegated_count as u32).to_le_bytes());
            bytes[40..44].copy_from_slice(&(dns.as_slice().len() as u32).to_le_bytes());
            if let Some(addresses) = entry.ipv6 {
                addresses.emit(&mut bytes[ENTRY_BYTES..delegated_offset]);
            }
            if let Some(prefixes) = entry.delegated {
                prefixes.emit(&mut bytes[delegated_offset..dns_offset]);
            }
            for (address, bytes) in dns
                .as_slice()
                .iter()
                .zip(bytes[dns_offset..].as_chunks_mut::<16>().0)
            {
                *bytes = address.octets();
            }
            offset = end;
        }
        Ok(size)
    }
    /// 验证完整版本、所有可变区间和每个意图；重复接口由事务 owner 拒绝。
    /// # Errors
    /// 旧版本、截断/尾部、保留位、隐藏数据或非法字段。
    pub fn parse(bytes: &'a [u8]) -> Result<Self, Invalid> {
        let header = bytes.get(..HEADER_BYTES).ok_or(Invalid)?;
        let count = u32_at(header, 4) as usize;
        if u32_at(header, 0) != VERSION || count == 0 {
            return Err(Invalid);
        }
        let body = &bytes[HEADER_BYTES..];
        let mut rest = body;
        for _ in 0..count {
            let size = entry_size(rest)?;
            let entry = rest.get(..size).ok_or(Invalid)?;
            if entry[4] & !127 != 0
                || entry[7] > 3
                || entry[4] & 2 == 0 && entry[8..12] != [0; 4]
                || entry[4] & 4 == 0
                    && (entry[6] != 0 || entry[12..16] != [0; 4] || entry[20..28] != [0; 8])
                || entry[4] & 8 == 0 && entry[16..20] != [0; 4]
                || entry[4] & 16 == 0 && u32_at(entry, 28) != 0
                || entry[4] & 64 == 0 && u32_at(entry, 36) != 0
                || entry[4] & 32 == 0 && entry[32..36] != [0; 4]
                || entry[32..34] == [0; 2] && (entry[34..36] != [0; 2] || u32_at(entry, 40) != 0)
            {
                return Err(Invalid);
            }
            if entry[4] & 16 != 0 {
                Ipv6Addresses::parse(&entry[ENTRY_BYTES..delegated_start(entry)])?;
            }
            decode(entry, decode_dns(entry)?).validate()?;
            rest = &rest[size..];
        }
        if !rest.is_empty() {
            return Err(Invalid);
        }
        Ok(Self { body, count })
    }
    /// 惰性借用已验证输入，数量准确且不再分配或检查可变 framing。
    pub fn iter(self) -> impl ExactSizeIterator<Item = InterfaceConfiguration<'a>> + Clone + 'a {
        Entries {
            body: self.body,
            remaining: self.count,
        }
    }
}
#[derive(Clone)]
struct Entries<'a> {
    body: &'a [u8],
    remaining: usize,
}
impl<'a> Iterator for Entries<'a> {
    type Item = InterfaceConfiguration<'a>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        let size = entry_size(self.body).expect("validated interface framing");
        let (entry, rest) = self.body.split_at(size);
        self.body = rest;
        self.remaining -= 1;
        Some(decode(
            entry,
            decode_dns(entry).expect("validated DNS advertisement"),
        ))
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}
impl ExactSizeIterator for Entries<'_> {}
fn entry_size(bytes: &[u8]) -> Result<usize, Invalid> {
    let header = bytes.get(..ENTRY_BYTES).ok_or(Invalid)?;
    (u32_at(header, 28) as usize)
        .checked_mul(ipv6::ROW)
        .and_then(|n| n.checked_add((u32_at(header, 36) as usize).checked_mul(delegated::ROW)?))
        .and_then(|n| n.checked_add((u32_at(header, 40) as usize).checked_mul(16)?))
        .and_then(|n| n.checked_add(ENTRY_BYTES))
        .ok_or(Invalid)
}
fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(
        bytes[offset..offset + 4]
            .try_into()
            .expect("validated field width"),
    )
}
fn delegated_start(bytes: &[u8]) -> usize {
    ENTRY_BYTES + u32_at(bytes, 28) as usize * ipv6::ROW
}

fn dns_start(bytes: &[u8]) -> usize {
    delegated_start(bytes) + u32_at(bytes, 36) as usize * delegated::ROW
}

fn decode_dns(bytes: &[u8]) -> Result<AdvertisedDns, Invalid> {
    let count = u32_at(bytes, 40) as usize;
    if count > MAX_ADVERTISED_DNS {
        return Err(Invalid);
    }
    let mut addresses = [Ipv6Addr::UNSPECIFIED; MAX_ADVERTISED_DNS];
    for (output, input) in addresses
        .iter_mut()
        .zip(bytes[dns_start(bytes)..].as_chunks::<16>().0)
    {
        *output = Ipv6Addr::from(*input);
    }
    AdvertisedDns::new(&addresses[..count])
}

fn decode(bytes: &[u8], dns: AdvertisedDns) -> InterfaceConfiguration<'_> {
    InterfaceConfiguration {
        index: u32_at(bytes, 0),
        up: bytes[4] & 1 != 0,
        ipv6: (bytes[4] & 16 != 0)
            .then(|| Ipv6Addresses::validated(&bytes[ENTRY_BYTES..delegated_start(bytes)])),
        delegated: (bytes[4] & 64 != 0).then(|| {
            DelegatedPrefixes::validated(&bytes[delegated_start(bytes)..dns_start(bytes)])
        }),
        accept_ra: match bytes[7] {
            0 => None,
            1 => Some(RouterAdvertisementPolicy::HostOnly),
            2 => Some(RouterAdvertisementPolicy::Disabled),
            3 => Some(RouterAdvertisementPolicy::Always),
            _ => unreachable!("validated RA policy"),
        },
        advertise: (bytes[4] & 32 != 0).then(|| {
            let max_interval_seconds = u16::from_le_bytes([bytes[32], bytes[33]]);
            if max_interval_seconds == 0 {
                RouterAdvertisements::Disabled
            } else {
                RouterAdvertisements::Enabled {
                    max_interval_seconds,
                    router_lifetime_seconds: u16::from_le_bytes([bytes[34], bytes[35]]),
                    dns,
                }
            }
        }),
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
