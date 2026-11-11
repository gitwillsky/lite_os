//! 单一网络配置捕获：接口与完整受管 FIB 共用同一正文、写租约及发布点。
use crate::routes::{
    NextHopConfiguration, RouteAddress, RouteConfiguration, RouteConfigurationBatch,
};
use alloc::vec::Vec;
use core::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use native_runtime::native_abi::{self, Status};
use router_protocol::interface::{InterfaceConfiguration, InterfaceConfigurationBatch};
use router_protocol::policy_routing::Rules;
use service_rpc::{Reader, Writer};

const VERSION: u32 = 5;
const HEADER: usize = 36;
const PROBE_BYTES: usize = 24;

fn probe(row: &[u8; PROBE_BYTES]) -> Result<(u32, IpAddr), Status> {
    let mut reader = Reader::new(row);
    let index = reader.u32()?;
    let address = match reader.u32()? {
        4 if row[12..] == [0; 12] => IpAddr::V4(Ipv4Addr::read(&mut reader)?),
        6 => IpAddr::V6(Ipv6Addr::read(&mut reader)?),
        _ => return Err(native_abi::ERR_INVALID_ARGS),
    };
    Ok((index, address))
}

fn valid_probe((index, address): (u32, IpAddr)) -> bool {
    index != 0
        && match address {
            IpAddr::V4(address) => matches!(address.octets()[0], 1..=126 | 128..=223),
            IpAddr::V6(address) => {
                !address.is_unspecified()
                    && !address.is_multicast()
                    && !address.is_loopback()
                    && address.to_ipv4_mapped().is_none()
            }
        }
}

/// 冻结网络变更；没有接口的批次可只替换 FIB，None FIB 表示不变，Some 空集合表示清除。
#[derive(Clone, Copy, Default)]
pub struct ConfigurationBatch<'a> {
    interfaces: InterfaceConfigurationBatch<'a>,
    forwarding: Option<RouteConfigurationBatch<'a, core::net::Ipv4Addr>>,
    ipv6: Option<RouteConfigurationBatch<'a, core::net::Ipv6Addr>>,
    probes: &'a [[u8; PROBE_BYTES]],
    rules: Option<Rules<'a>>,
    prerouting: Option<router_protocol::firewall::Prerouting<'a>>,
}
impl<'a> ConfigurationBatch<'a> {
    /// 编码只含接口的变更，保持受管路由；所有正文仍由唯一 encoder 生成。
    /// # Parameters
    /// `interfaces` 为非空接口计划，不另建 FIB。
    /// # Errors
    /// 空变更、字段/长度非法或资源不足。
    pub fn encode(interfaces: &[InterfaceConfiguration]) -> Result<Vec<u8>, Status> {
        Ok(Self::encoder(interfaces, 0, 0, 0, 0)?.into_bytes())
    }
    /// 从冻结借用投影编码接口与完整 FIB，不收集第二份逐前缀 DTO 数组。
    /// # Parameters
    /// 两族的 hops/routes 在克隆/两次遍历间保持同一数量与值；空集合明确清除该族受管 FIB。
    /// IPv6 同前缀路由按优先顺序提供；排序取决于控制面的策略，不由 metric 重新推导。
    /// `probes` 为有序且不重复的双栈失败依赖，随对应 FIB 发布，无权新建邻居或安装路由。
    /// `interfaces` 可为空；其字段仍由原接口 codec 验证。
    /// `rules` 与两族 FIB 同步替换，缺席时保持。
    /// # Returns
    /// 只分配一次最终正文，全部字段复用同一 FIB codec。
    /// # Errors
    /// 字段/数量/长度非法或资源不足；不返回部分正文。
    pub fn encode_forwarding<'b>(
        interfaces: &[InterfaceConfiguration],
        hops: impl Clone + ExactSizeIterator<Item = NextHopConfiguration<'b, core::net::Ipv4Addr>>,
        routes: impl Clone + ExactSizeIterator<Item = RouteConfiguration<core::net::Ipv4Addr>>,
        ipv6_hops: impl Clone + ExactSizeIterator<Item = NextHopConfiguration<'b, core::net::Ipv6Addr>>,
        ipv6_routes: impl Clone + ExactSizeIterator<Item = RouteConfiguration<core::net::Ipv6Addr>>,
        probes: &[(u32, IpAddr)],
        rules: Option<Rules<'_>>,
    ) -> Result<Vec<u8>, Status> {
        let route_length = RouteConfigurationBatch::encoded_size(hops.clone(), routes.clone())?;
        let ipv6_length =
            RouteConfigurationBatch::encoded_size(ipv6_hops.clone(), ipv6_routes.clone())?;
        if probes.iter().any(|entry| !valid_probe(*entry))
            || !probes.is_sorted_by(|left, right| left < right)
        {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        let rule_bytes = rules.map_or(&[][..], Rules::bytes);
        let mut writer = Self::encoder(
            interfaces,
            route_length,
            ipv6_length,
            probes.len(),
            rule_bytes.len(),
        )?;
        RouteConfigurationBatch::write_fields(&mut writer, hops, routes);
        RouteConfigurationBatch::write_fields(&mut writer, ipv6_hops, ipv6_routes);
        for (index, address) in probes {
            writer.u32(*index);
            match address {
                IpAddr::V4(address) => {
                    writer.u32(4);
                    address.write(&mut writer);
                    writer.u64(0).u32(0);
                }
                IpAddr::V6(address) => {
                    writer.u32(6);
                    address.write(&mut writer);
                }
            }
        }
        let mut bytes = writer.into_bytes();
        bytes.extend_from_slice(rule_bytes);
        Ok(bytes)
    }
    fn encoder(
        interfaces: &[InterfaceConfiguration],
        route_length: usize,
        ipv6_length: usize,
        probes: usize,
        rules: usize,
    ) -> Result<Writer, Status> {
        if interfaces.is_empty() && route_length == 0 && ipv6_length == 0 {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        let interface_length = if interfaces.is_empty() {
            0
        } else {
            InterfaceConfigurationBatch::encoded_size(interfaces)
                .map_err(|_| native_abi::ERR_INVALID_ARGS)?
        };
        let prefix = HEADER
            .checked_add(interface_length)
            .ok_or(native_abi::ERR_OUT_OF_RANGE)?;
        let length = prefix
            .checked_add(route_length)
            .and_then(|length| length.checked_add(ipv6_length))
            .and_then(|length| length.checked_add(probes.checked_mul(PROBE_BYTES)?))
            .and_then(|length| length.checked_add(rules))
            .ok_or(native_abi::ERR_OUT_OF_RANGE)?;
        let probe_count = u32::try_from(probes).map_err(|_| native_abi::ERR_OUT_OF_RANGE)?;
        let rule_length = u32::try_from(rules).map_err(|_| native_abi::ERR_OUT_OF_RANGE)?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(length)
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        bytes.resize(prefix, 0);
        bytes[..4].copy_from_slice(&VERSION.to_le_bytes());
        bytes[4..12].copy_from_slice(&(interface_length as u64).to_le_bytes());
        bytes[12..20].copy_from_slice(&(route_length as u64).to_le_bytes());
        bytes[20..28].copy_from_slice(&(ipv6_length as u64).to_le_bytes());
        bytes[28..32].copy_from_slice(&probe_count.to_le_bytes());
        bytes[32..36].copy_from_slice(&rule_length.to_le_bytes());
        if interface_length != 0 {
            InterfaceConfigurationBatch::encode(interfaces, &mut bytes[HEADER..])
                .map_err(|_| native_abi::ERR_INVALID_ARGS)?;
        }
        Ok(Writer::from_bytes(bytes))
    }
    /// 完整验证外层长度、接口及两族 FIB 正文；所有引用只借用这一份已捕获的不可变字节。
    /// # Errors
    /// 未知版本、空变更、长度/字段错误、截断或包尾。
    pub fn parse(bytes: &'a [u8]) -> Result<Self, Status> {
        let mut reader = Reader::new(bytes);
        if reader.u32()? != VERSION {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        let interface_length =
            usize::try_from(reader.u64()?).map_err(|_| native_abi::ERR_OUT_OF_RANGE)?;
        let route_length =
            usize::try_from(reader.u64()?).map_err(|_| native_abi::ERR_OUT_OF_RANGE)?;
        let ipv6_length =
            usize::try_from(reader.u64()?).map_err(|_| native_abi::ERR_OUT_OF_RANGE)?;
        let probe_count = reader.u32()? as usize;
        let rule_length = reader.u32()? as usize;
        let split = HEADER
            .checked_add(interface_length)
            .ok_or(native_abi::ERR_INVALID_ARGS)?;
        let ipv6_start = split
            .checked_add(route_length)
            .ok_or(native_abi::ERR_INVALID_ARGS)?;
        let probe_start = ipv6_start
            .checked_add(ipv6_length)
            .ok_or(native_abi::ERR_INVALID_ARGS)?;
        let rule_start = probe_count
            .checked_mul(PROBE_BYTES)
            .and_then(|length| probe_start.checked_add(length))
            .ok_or(native_abi::ERR_INVALID_ARGS)?;
        if interface_length == 0 && route_length == 0 && ipv6_length == 0
            || rule_start.checked_add(rule_length) != Some(bytes.len())
            || rule_length != 0 && (route_length == 0 || ipv6_length == 0)
        {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        let (probes, _) = bytes[probe_start..rule_start].as_chunks::<PROBE_BYTES>();
        let mut previous = None;
        for row in probes {
            let entry = probe(row)?;
            if !valid_probe(entry)
                || previous.is_some_and(|previous| previous >= entry)
                || if entry.1.is_ipv4() {
                    route_length == 0
                } else {
                    ipv6_length == 0
                }
            {
                return Err(native_abi::ERR_INVALID_ARGS);
            }
            previous = Some(entry);
        }
        let interface_bytes = bytes
            .get(HEADER..split)
            .ok_or(native_abi::ERR_INVALID_ARGS)?;
        let interfaces = if interface_bytes.is_empty() {
            InterfaceConfigurationBatch::default()
        } else {
            InterfaceConfigurationBatch::parse(interface_bytes)
                .map_err(|_| native_abi::ERR_INVALID_ARGS)?
        };
        let forwarding = if route_length == 0 {
            None
        } else {
            Some(RouteConfigurationBatch::parse(&bytes[split..ipv6_start])?)
        };
        let ipv6 = if ipv6_length == 0 {
            None
        } else {
            Some(RouteConfigurationBatch::parse(
                &bytes[ipv6_start..probe_start],
            )?)
        };
        let rules = if rule_length == 0 {
            None
        } else {
            Some(Rules::parse(&bytes[rule_start..]).map_err(|_| native_abi::ERR_INVALID_ARGS)?)
        };
        Ok(Self {
            interfaces,
            forwarding,
            ipv6,
            probes,
            rules,
            prerouting: None,
        })
    }
    /// 合并预备帧携带的预路由链，和同批接口与路由一起发布。
    pub fn with_prerouting(mut self, chain: router_protocol::firewall::Prerouting<'a>) -> Self {
        self.prerouting = Some(chain);
        self
    }

    /// 借用同批预路由规则；缺席保持。
    pub fn prerouting(self) -> Option<router_protocol::firewall::Prerouting<'a>> {
        self.prerouting
    }

    /// 无分配借用接口计划；没有接口变更时迭代器为空。
    pub fn interfaces(self) -> InterfaceConfigurationBatch<'a> {
        self.interfaces
    }
    /// 可选的完整 FIB 替换；None 不改变原受管集合，Some 空集合撤销该来源。
    pub fn forwarding(self) -> Option<RouteConfigurationBatch<'a, core::net::Ipv4Addr>> {
        self.forwarding
    }
    /// 可选的 IPv6 受管集合；缺席保持，显式空集合撤销，不影响同批 IPv4 的决定。
    pub fn ipv6_forwarding(self) -> Option<RouteConfigurationBatch<'a, core::net::Ipv6Addr>> {
        self.ipv6
    }

    /// 借用本批次需持续复探的失败网关。顺序按接口和地址，无重复。
    pub fn gateway_probes(self) -> impl ExactSizeIterator<Item = (u32, IpAddr)> + Clone + 'a {
        self.probes
            .iter()
            .map(|row| probe(row).expect("validated gateway probe"))
    }

    /// 借用同批替换的选表规则；空正文清除规则，None 保持。
    pub fn rules(self) -> Option<Rules<'a>> {
        self.rules
    }
}
