//! RFC 4271 Phase 1：只借用原始共享属性，生成后政策 degree/下一跳；可达性仍由实际 RIB 提供。
use crate::{AttributeChanges, NextHop, Peer, PeerKind, ReceivedRoute, RouteAttributes};
use packet_wire::bgp::nlri::Prefix;
use router_protocol::{
    bgp::{ImportPolicy, RouteAction, RouteRule},
    policy::Disposition,
};

/// 入口政策的逻辑决定；属性改写须经同域共享池投影，不能直接改原始接收表。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImportDecision<'a> {
    /// 后政策逻辑下一跳，不代表已经可达。
    pub next_hop: NextHop,
    /// 本地 degree，零仍有效，不是管理距离。
    pub preference: u32,
    /// 借用同一已校验配置的属性编辑，不复制规则中的集合。
    // OWNER: 冻结配置中的唯一动作借用；临时候选只需一个指针，避免每前缀复制三个集合描述符。
    rule: Option<&'a RouteAction>,
}
impl<'a> ImportDecision<'a> {
    /// 只读投影本次已获准动作，集合仍借用唯一配置，不分配或持有缓存。
    pub fn changes(self) -> AttributeChanges<'a> {
        AttributeChanges::permitted(self.rule)
    }
}
/// 按显式入口策略的首个匹配规则分类；未配置策略一律拒绝，不能当作默认 permit。
/// # Parameters
/// 输入属于同一邻居的规范前缀和已裁决原始路由；调用者须先验证持久策略，不在逐路由路径重复分配或验证。
/// # Returns
/// 接纳时返回后政策逻辑下一跳、degree 与借用的属性编辑，拒绝返回 None；不修改原始属性，也不声称下一跳可达。
pub fn import_route<'a>(
    policy: Option<&'a ImportPolicy>,
    prefix: Prefix,
    peer: &Peer,
    route: &ReceivedRoute,
) -> Option<ImportDecision<'a>> {
    let policy = policy?;
    let attributes = route.attributes();
    let preference = if peer.kind != PeerKind::External {
        attributes.local_preference().unwrap_or(policy.preference)
    } else {
        policy.preference
    };
    let rule = action(&policy.rules, prefix, attributes, preference);
    match rule {
        Some(RouteAction::Deny) => None,
        Some(RouteAction::Permit {
            preference: override_preference,
            next_hop,
            ..
        }) if next_hop.is_none_or(|address| prefix.address.is_ipv4() || address.is_ipv6()) => {
            Some(ImportDecision {
                next_hop: next_hop.map_or_else(
                    || route.next_hop(),
                    |address| NextHop {
                        address,
                        link_local: None,
                    },
                ),
                preference: override_preference.unwrap_or(preference),
                rule,
            })
        }
        Some(RouteAction::Permit { .. }) => None,
        None => (policy.default == Disposition::Permit).then(|| ImportDecision {
            next_hop: route.next_hop(),
            preference,
            rule: None,
        }),
    }
}

pub(crate) fn action<'a>(
    rules: &'a [RouteRule],
    prefix: Prefix,
    attributes: &RouteAttributes,
    preference: u32,
) -> Option<&'a RouteAction> {
    // 每次分类只在确有长度条件时计算一次；属性不可变，不为每个前缀持久缓存派生跳数。
    let mut path_length = None;
    for rule in rules {
        if rule
            .prefix
            .as_ref()
            .is_some_and(|condition| !condition.matches(prefix.address, prefix.length))
            || rule
                .origin
                .is_some_and(|origin| attributes.origin() != origin as u8)
            || rule
                .med
                .as_ref()
                .is_some_and(|range| attributes.med().is_none_or(|value| !range.contains(&value)))
            || rule
                .local_preference
                .as_ref()
                .is_some_and(|range| !range.contains(&preference))
            || rule.as_path_length.as_ref().is_some_and(|range| {
                !range.contains(path_length.get_or_insert_with(|| {
                    u32::try_from(attributes.path().hop_count())
                        .expect("stored AS path uses u32 indices")
                }))
            })
            || rule
                .as_path_contains
                .iter()
                .any(|asn| !attributes.path().contains(asn.get()))
            || rule
                .as_path_regex
                .as_ref()
                .is_some_and(|expression| !expression.is_match(attributes.path()))
            || rule
                .communities
                .iter()
                .any(|value| attributes.communities().binary_search(value).is_err())
            || rule.extended_communities.iter().any(|value| {
                attributes
                    .extended_communities()
                    .binary_search(value)
                    .is_err()
            })
            || rule
                .large_communities
                .iter()
                .any(|value| attributes.large_communities().binary_search(value).is_err())
        {
            continue;
        }
        return Some(&rule.action);
    }
    None
}
