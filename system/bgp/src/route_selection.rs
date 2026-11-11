//! 单批入口分类、真实 RIB 反馈和来源确认；提案永不提交到本地表，已验证选择只在远端确认后发表。
use alloc::vec::Vec;
use bgp::{
    ImportDecision, Reachability, RouteAttributes, RoutingDomain, RoutingError, SourceError,
    import_route,
};
use core::num::NonZeroU32;
use native_runtime::native_abi::{self, Status};
use network_protocol::routing::RoutingSource;
use packet_wire::bgp::nlri::{Family, Prefix};
use router_protocol::{bgp::Neighbor, tracking::installation::Outcome};

struct Classified<'a> {
    prefix: Prefix,
    peer: u64,
    decision: ImportDecision<'a>,
    attributes: RouteAttributes,
    resolve_via_default: bool,
}
fn routing(error: RoutingError) -> Status {
    match error {
        RoutingError::NoMemory => native_abi::ERR_NO_MEMORY,
        RoutingError::Busy => native_abi::ERR_SHOULD_WAIT,
        _ => native_abi::ERR_BAD_STATE,
    }
}
fn encoding(error: SourceError) -> Status {
    match error {
        SourceError::NoMemory => native_abi::ERR_NO_MEMORY,
        _ => native_abi::ERR_BAD_STATE,
    }
}
fn execution(error: network_protocol::SocketError) -> Status {
    match error {
        network_protocol::SocketError::NoMemory => native_abi::ERR_NO_MEMORY,
        _ => native_abi::ERR_BAD_STATE,
    }
}
fn entry(classified: &[Classified<'_>], prefix: Prefix, peer: u64) -> Option<usize> {
    classified
        .binary_search_by(|entry| (entry.prefix, entry.peer).cmp(&(prefix, peer)))
        .ok()
}
/// 原生 actor 的一次完整来源重算；bindings 只借用仍注册的实际 TCP 身份与唯一配置。
pub(super) fn reconcile(
    routes: &mut RoutingDomain,
    source: &RoutingSource,
    minimum: NonZeroU32,
    bindings: &[(u64, &Neighbor)],
) -> Result<Vec<packet_wire::bgp::nlri::Prefix>, Status> {
    routes.reconsider_all().map_err(routing)?;
    let mut changed = Vec::new();
    // IPv4 的逻辑下一跳可依赖本次学习的 IPv6 路由，先提交底层族。
    for family in [Family::Ipv6, Family::Ipv4] {
        if !routes
            .pending()
            .iter()
            .any(|prefix| prefix.address.is_ipv4() == (family == Family::Ipv4))
        {
            continue;
        }
        let next = reconcile_family(routes, source, minimum, bindings, family)?;
        changed
            .try_reserve(next.len())
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        changed.extend(next);
    }
    Ok(changed)
}
fn reconcile_family(
    routes: &mut RoutingDomain,
    source: &RoutingSource,
    minimum: NonZeroU32,
    bindings: &[(u64, &Neighbor)],
    family: Family,
) -> Result<Vec<Prefix>, Status> {
    let mut classified = Vec::new();
    for (peer, received) in routes.peers() {
        let parameters = bindings
            .binary_search_by_key(&peer.id, |(id, _)| *id)
            .ok()
            .map(|index| bindings[index].1)
            .ok_or(native_abi::ERR_BAD_STATE)?;
        if parameters.import.is_none() {
            continue;
        }
        for (prefix, route) in received.iter() {
            if prefix.address.is_ipv4() != (family == Family::Ipv4) {
                continue;
            }
            if let Some(decision) = import_route(parameters.import.as_ref(), prefix, peer, route) {
                classified
                    .try_reserve(1)
                    .map_err(|_| native_abi::ERR_NO_MEMORY)?;
                classified.push(Classified {
                    prefix,
                    peer: peer.id,
                    decision,
                    attributes: route.attributes().clone(),
                    resolve_via_default: parameters.resolve_via_default,
                });
            }
        }
    }
    for item in &mut classified {
        item.attributes = routes
            .project_attributes(&item.attributes, item.decision.changes())
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    }
    classified.sort_unstable_by_key(|entry| (entry.prefix, entry.peer));
    let mut probes = Vec::new();
    probes
        .try_reserve_exact(classified.len())
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    for item in &classified {
        probes.push(
            routes
                .installation(
                    item.prefix,
                    item.peer,
                    item.decision.next_hop,
                    item.resolve_via_default,
                )
                .map_err(encoding)?,
        );
    }
    // 1. 尚未验证的提案保留 Unresolved；只提供给真实 RIB 求解，不伪造可达或直接提交。
    let mut encoded = routes
        .propose(family, |prefix, peer, route| {
            Ok(entry(&classified, prefix, peer.id).map_or_else(
                || (route.next_hop(), None, route.attributes()),
                |index| {
                    (
                        classified[index].decision.next_hop,
                        Some(classified[index].decision.preference),
                        &classified[index].attributes,
                    )
                },
            ))
        })
        .map_err(routing)?
        .encode_source(minimum)
        .map_err(encoding)?;
    // OWNER: Brent 环检测只保留一个临时来源检查点，不积累全表历史；重复提案不能在事件线程无限忙转。
    let mut checkpoint = Vec::new();
    let mut generation = None;
    let mut power = 1usize;
    let mut distance = 0usize;
    loop {
        let prepared = source.prepare(&encoded, &[], &probes)?;
        let current_generation = prepared.snapshot().generation();
        if generation != Some(current_generation) {
            checkpoint.clear();
            checkpoint
                .try_reserve(encoded.len())
                .map_err(|_| native_abi::ERR_NO_MEMORY)?;
            checkpoint.extend_from_slice(&encoded);
            generation = Some(current_generation);
            power = 1;
            distance = 0;
        }
        let outcomes = prepared.outcomes();
        // 2. 只用当前候选图返回的可达证明选择；原始接收属性保持不变。
        let update = routes
            .prepare(family, |prefix, peer, route| {
                Ok(entry(&classified, prefix, peer.id).map_or_else(
                    || {
                        (
                            route.next_hop(),
                            None,
                            Reachability::Unresolved,
                            route.attributes(),
                        )
                    },
                    |index| {
                        let Outcome {
                            reachable,
                            interior_cost,
                        } = outcomes[index];
                        let item = &classified[index];
                        (
                            item.decision.next_hop,
                            Some(item.decision.preference),
                            if reachable {
                                Reachability::Resolved { interior_cost }
                            } else {
                                Reachability::Unresolved
                            },
                            &item.attributes,
                        )
                    },
                ))
            })
            .map_err(routing)?;
        let next = update.encode_source(minimum).map_err(encoding)?;
        if next == encoded {
            // 3. 来源与证明一致才发表。只有选中路径或复探义务改变才确认，避免通知自激。
            if update.changed_prefixes().is_empty() && !prepared.recovery_changed() {
                prepared.abort().map_err(execution)?;
            } else {
                if let Err(error) = prepared.activate() {
                    let _ = prepared.abort();
                    return Err(execution(error));
                }
                prepared.confirm().map_err(execution)?;
            }
            return Ok(update.commit());
        }
        drop(update);
        prepared.abort().map_err(execution)?;
        if next == checkpoint {
            native_runtime::error!("BGP recursive route selection did not stabilize");
            return Err(native_abi::ERR_BAD_STATE);
        }
        distance = distance
            .checked_add(1)
            .ok_or(native_abi::ERR_NO_RESOURCES)?;
        if distance == power {
            checkpoint.clear();
            checkpoint
                .try_reserve(next.len())
                .map_err(|_| native_abi::ERR_NO_MEMORY)?;
            checkpoint.extend_from_slice(&next);
            power = power.checked_mul(2).ok_or(native_abi::ERR_NO_RESOURCES)?;
            distance = 0;
        }
        encoded = next;
    }
}
