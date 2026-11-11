//! Loc-RIB 候选到既有来源正文的投影；不另建 wire 布局、来源身份或发布状态。
use crate::{LocRibUpdate, OutOfMemory, SelectedRoute};
use alloc::vec::Vec;
use core::{net::IpAddr, num::NonZeroU32};
use fallible_tree::FallibleMap;
use packet_wire::bgp::nlri::Family;
use router_protocol::routing::{self, Attributes, Batch, Member, Target};

/// 一个稳定邻居在当前来源授权中的只读投影；由配置/会话 owner 在整批编码期间冻结。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourcePeer {
    /// 原生管理距离，越小越优；不是 BGP LOCAL_PREF、MED 或 AS_PATH 长度。
    pub preference: NonZeroU32,
    /// 已确认与所采用 link-local 下一跳共链路的运行时接口；没有这种事实必须为 None。
    /// 不能从对端 ASN、IPv6 地址或多跳 TCP 会话猜测；接口及下一跳准入仍由统一 RIB 检查。
    pub next_hop_interface: Option<NonZeroU32>,
}

/// 候选来源未编码的原因；失败不改变 Loc-RIB，也不向统一 RIB 发送半个声明。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceError {
    /// 字典或最终正文分配失败。
    NoMemory,
    /// 选择结果的稳定邻居已经没有相应的来源配置。
    MissingPeer,
    /// 配置距离优先级高于来源能力授予的范围。
    Preference,
    /// 只有 link-local 下一跳，却没有真实接口范围。
    Scope,
    /// 下一跳、族、引用或大小不能被既有来源协议表示。
    Invalid,
}
impl From<OutOfMemory> for SourceError {
    fn from(_: OutOfMemory) -> Self {
        Self::NoMemory
    }
}
impl From<routing::EncodeError> for SourceError {
    fn from(error: routing::EncodeError) -> Self {
        match error {
            routing::EncodeError::NoMemory => Self::NoMemory,
            routing::EncodeError::Invalid => Self::Invalid,
        }
    }
}

impl SourcePeer {
    pub(crate) fn target(self, hop: crate::NextHop) -> Result<Target, SourceError> {
        if hop.address.is_unspecified()
            || hop.address.is_multicast()
            || matches!(hop.address, IpAddr::V4(address) if address.is_broadcast())
            || hop.link_local.is_some_and(|address| {
                !address.is_unicast_link_local() || !matches!(hop.address, IpAddr::V6(_))
            })
        {
            return Err(SourceError::Invalid);
        }
        let local = hop.link_local.or_else(|| match hop.address {
            IpAddr::V6(address) if address.is_unicast_link_local() => Some(address),
            _ => None,
        });
        Ok(match (local, self.next_hop_interface) {
            (Some(address), Some(interface)) => Target::Direct {
                interface_index: interface.get(),
                gateway: Some(address.into()),
                on_link: true,
            },
            _ if matches!(hop.address, IpAddr::V6(address) if address.is_unicast_link_local()) => {
                return Err(SourceError::Scope);
            }
            _ => Target::Recursive(hop.address),
        })
    }
}

fn project(
    route: &SelectedRoute,
    minimum: NonZeroU32,
    peers: &FallibleMap<u64, SourcePeer>,
) -> Result<(Attributes, Target), SourceError> {
    let peer = peers.get(&route.peer()).ok_or(SourceError::MissingPeer)?;
    if peer.preference < minimum {
        return Err(SourceError::Preference);
    }
    let target = peer.target(route.next_hop())?;
    Ok((
        Attributes {
            preference: peer.preference.get(),
            // 每个前缀只发表一个 BGP 赢家；协议内优先级已裁决。MED 仅能在相邻 AS 范围比较，不能泄漏为全局代价。
            metric: 0,
        },
        target,
    ))
}

fn intern<K: Ord + Copy, V>(
    index: &mut FallibleMap<K, u32>,
    values: &mut Vec<V>,
    value: K,
    capture: impl FnOnce(K) -> V,
) -> Result<u32, SourceError> {
    if let Some(index) = index.get(&value) {
        return Ok(*index);
    }
    let slot = u32::try_from(values.len()).map_err(|_| SourceError::Invalid)?;
    values.try_reserve(1).map_err(|_| SourceError::NoMemory)?;
    index.try_insert(value, slot)?;
    values.push(capture(value));
    Ok(slot)
}

impl LocRibUpdate<'_> {
    /// 将本族完整候选视图编码为既有统一 RIB 来源正文；不提交/消费 guard 或发送 RPC。
    /// # Parameters
    /// `minimum` 来自实际来源授权，`peers` 按稳定邻居身份提供冻结管理距离与真实接口范围。
    /// # Returns
    /// 单份完整正文；调用者沿同一来源能力替换，确认成功后再消费本 guard 的 commit。
    /// # Errors
    /// 邻居缺失、距离越权、缺少 link-local 范围、不可表示的字段或内存不足；旧表不变。
    pub fn encode_source(
        &self,
        minimum: NonZeroU32,
        peers: &FallibleMap<u64, SourcePeer>,
    ) -> Result<Vec<u8>, SourceError> {
        // 1. 临时索引只为不同属性/目标分配；它们不跨批次存活，也不成为另一份路由状态。
        let mut attribute_index = FallibleMap::new();
        let mut attributes = Vec::new();
        let mut hop_index = FallibleMap::new();
        let mut members = Vec::new();
        for (_, route) in self.iter() {
            let (attribute, target) = project(route, minimum, peers)?;
            intern(&mut attribute_index, &mut attributes, attribute, |value| {
                value
            })?;
            intern(&mut hop_index, &mut members, target, |target| {
                [Member { target, weight: 1 }]
            })?;
        }
        // 2. 唯一赢家的单成员组权重为 1；codec 重放冻结视图精确预留最终正文，不复制组视图或逐前缀 DTO。
        Ok(Batch::encode(
            match self.family() {
                Family::Ipv4 => routing::Family::Ipv4,
                Family::Ipv6 => routing::Family::Ipv6,
            },
            &attributes,
            members
                .iter()
                .map(|members| routing::NextHop::new(routing::Action::Forward, members)),
            self.iter().map(|(prefix, route)| {
                let (attribute, target) = project(route, minimum, peers)
                    .expect("same frozen source projection passed dictionary capture");
                routing::Route {
                    network: prefix.address,
                    length: prefix.length,
                    attributes: *attribute_index.get(&attribute).expect("captured attribute"),
                    next_hop: *hop_index.get(&target).expect("captured next hop"),
                }
            }),
        )?)
    }
}
