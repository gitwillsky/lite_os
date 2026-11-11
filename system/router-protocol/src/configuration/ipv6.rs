//! 静态 IPv6 地址与显式 on-link 意图；地址身份本身不隐含前缀在链路上。
use super::*;
use core::net::Ipv6Addr;

fn ipv6_schema(_: &mut SchemaGenerator) -> Schema {
    json_schema!({"type":"string", "format":"ipv6"})
}
fn on_link() -> bool {
    true
}
/// 一个静态 IPv6 地址；生效仍须由执行 owner 完成 DAD。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Ipv6Address {
    #[schemars(schema_with = "ipv6_schema")]
    pub address: Ipv6Addr,
    #[schemars(range(max = 128))]
    pub prefix_length: u8,
    /// 是否显式安装此前缀的直连可达意图；false 不允许从地址推断邻居直达。
    #[serde(default = "on_link")]
    pub on_link: bool,
}
impl Ipv6Address {
    /// 检查以太网单播静态地址的表示；接口冲突及自动地址重复由执行 owner 检查。
    pub fn valid(self) -> bool {
        self.prefix_length <= 128
            && !self.address.is_unspecified()
            && !self.address.is_multicast()
            && !self.address.is_loopback()
            && self.address.to_ipv4_mapped().is_none()
    }

    /// 可发布给下游主机的静态 SLAAC 前缀。
    pub fn advertised_prefix(self) -> Option<Ipv6Addr> {
        if !self.on_link || self.prefix_length != 64 || self.address.is_unicast_link_local() {
            return None;
        }
        let network = Ipv6Addr::from(u128::from(self.address) & (u128::MAX << 64));
        (!network.is_unspecified()).then_some(network)
    }
}
/// IPv6 接口的路由通告接收及路由请求发送策略。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RouterAdvertisementPolicy {
    /// 仅当 IPv6 转发关闭时接受通告。
    #[default]
    HostOnly,
    /// 停止接收通告及发送路由请求。
    Disabled,
    /// 转发开启时也接受通告，适用于通过上联自动配置的路由器。
    Always,
}

/// 最小 IPv6 MTU 扣除 IPv6、RA、源 MAC 与 MTU 选项后的空间。
pub const ADVERTISEMENT_OPTION_SPACE: usize = 1280 - 40 - 16 - 8 - 8;
/// 没有其他可选信息时一个 RA 可容纳的前缀上限。
pub const MAX_ADVERTISED_PREFIXES: usize = ADVERTISEMENT_OPTION_SPACE / 32;

/// 下游静态路由通告。前缀由同一接口的已启用直连 /64 地址提供。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum RouterAdvertisements {
    #[default]
    Disabled,
    Enabled {
        #[serde(default = "max_interval")]
        #[schemars(range(min = 4, max = 1800))]
        max_interval_seconds: u16,
        /// 零表示只发布前缀，不公布默认网关。
        #[serde(default = "router_lifetime")]
        #[schemars(range(max = 9000))]
        router_lifetime_seconds: u16,
        /// 通过 RDNSS 发布的递归 DNS，寿命为最大通告间隔的三倍。
        #[serde(default)]
        dns: AdvertisedDns,
    },
}

fn max_interval() -> u16 {
    600
}
fn router_lifetime() -> u16 {
    1800
}

impl RouterAdvertisements {
    /// 本配置需要发布的 DNS 服务器；停止通告时为空。
    pub fn dns(self) -> AdvertisedDns {
        match self {
            Self::Disabled => AdvertisedDns::default(),
            Self::Enabled { dns, .. } => dns,
        }
    }

    /// 扣除 RDNSS、源 MAC 和 MTU 选项后，一个最小 MTU 通告可容纳的前缀数。
    pub fn max_prefixes(self) -> usize {
        (ADVERTISEMENT_OPTION_SPACE - rdnss_size(self.dns().as_slice().len())) / 32
    }

    /// 验证通告间隔和默认网关寿命的关系。
    pub fn valid(self) -> bool {
        match self {
            Self::Disabled => true,
            Self::Enabled {
                max_interval_seconds,
                router_lifetime_seconds,
                ..
            } => {
                (4..=1800).contains(&max_interval_seconds)
                    && (router_lifetime_seconds == 0
                        || (max_interval_seconds..=9000).contains(&router_lifetime_seconds))
            }
        }
    }
}

/// 一个接口的 IPv6 意图；空集合清除手动地址，自动链路地址保持独立。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Ipv6Configuration {
    #[serde(default, deserialize_with = "addresses")]
    pub addresses: Vec<Ipv6Address>,
    #[serde(default)]
    pub accept_ra: RouterAdvertisementPolicy,
    #[serde(default)]
    pub advertise: RouterAdvertisements,
}
fn addresses<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<Ipv6Address>, D::Error> {
    let mut values = Vec::<Ipv6Address>::deserialize(deserializer)?;
    values.sort_unstable_by_key(|value| value.address);
    if values
        .windows(2)
        .any(|pair| pair[0].address == pair[1].address)
    {
        return Err(serde::de::Error::custom("duplicate IPv6 address"));
    }
    Ok(values)
}
impl Ipv6Configuration {
    pub(super) fn validate(&self) -> Result<(), ValidationError> {
        for (index, value) in self.addresses.iter().enumerate() {
            if !value.valid() || index != 0 && self.addresses[index - 1].address >= value.address {
                return Err(invalid(
                    format!("/ipv6/addresses/{index}"),
                    "invalid or noncanonical IPv6 address",
                ));
            }
        }
        if !self.advertise.valid()
            || self.advertise != RouterAdvertisements::Disabled
                && !valid_advertised_prefixes(
                    self.addresses.iter().copied(),
                    self.advertise.max_prefixes(),
                )
        {
            return Err(invalid(
                "/ipv6/advertise",
                "invalid advertisement lifetime, interval or too many prefixes",
            ));
        }
        Ok(())
    }
}

/// 校验按地址排序的候选集合能放入一个最小 MTU 通告。
pub fn valid_advertised_prefixes(
    addresses: impl Iterator<Item = Ipv6Address>,
    maximum: usize,
) -> bool {
    let mut previous = None;
    let mut count = 0;
    for prefix in addresses.filter_map(Ipv6Address::advertised_prefix) {
        if previous != Some(prefix) {
            previous = Some(prefix);
            count += 1;
        }
    }
    count <= maximum
}
