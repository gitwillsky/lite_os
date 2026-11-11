//! 本机重组超时/格式错误使用原 FIB、错误限速和回复队列，不经过转发策略开关。
use super::{Problem, packet};
use crate::inet::{NetworkStack, route_reply::RouteReplies};
use alloc::vec::Vec;
use core::net::IpAddr;
use smoltcp::time::Instant;
impl NetworkStack {
    pub(in crate::inet) fn retire_fragments(&mut self, now: Instant) -> bool {
        for _ in 0..16 {
            let Some(outcome) = self.reassembly.retire(now, |index| {
                self.interfaces
                    .iter()
                    .any(|entry| entry.index == index && entry.state.up)
            }) else {
                return false;
            };
            if let super::Outcome::Problem(problem) = outcome {
                let local = packet::error_flow(&problem.quote, problem.reason).is_some_and(
                    |(source, _)| match source {
                        IpAddr::V4(address) => self.routing.is_local(address, &self.interfaces),
                        IpAddr::V6(address) => self.interfaces.iter().any(|entry| {
                            (!address.is_unicast_link_local() || entry.index == problem.interface)
                                && entry.has_ipv6_address(address)
                        }),
                    },
                );
                if let Some(origin) = self
                    .interfaces
                    .iter_mut()
                    .find(|entry| entry.index == problem.interface)
                {
                    respond(
                        problem,
                        &mut origin.error_budget,
                        &mut self.route_replies,
                        now,
                        |_| local,
                    );
                }
            }
        }
        self.reassembly.due(now)
    }
}
/// 仅准入且仍为本机的目的可以作为错误源；最终路由/邻居/MTU 沿原回复队列再次验证。
pub(in crate::inet) fn respond(
    problem: Problem,
    errors: &mut crate::inet::route_reply::ErrorBudget,
    replies: &mut RouteReplies,
    now: Instant,
    local: impl FnOnce(IpAddr) -> bool,
) {
    let Some((source, destination)) = packet::error_flow(&problem.quote, problem.reason) else {
        return;
    };
    if !local(source) {
        return;
    }
    let bound = matches!(destination, IpAddr::V6(ip) if ip.is_unicast_link_local())
        .then_some(problem.interface);
    if !errors.spend(now.total_millis().max(0) as u64) {
        return;
    }
    let (header, maximum) = if source.is_ipv4() {
        (20, 548)
    } else {
        (40, 1232)
    };
    let quote = &problem.quote[..problem.quote.len().min(maximum)];
    if quote.len() < if source.is_ipv4() { 20 } else { 40 } {
        return;
    }
    let mut bytes = Vec::new();
    if bytes.try_reserve_exact(header + 8 + quote.len()).is_err() {
        return;
    }
    bytes.resize(header + 8 + quote.len(), 0);
    let (kind, code, parameter) = problem.reason;
    let result = match (source, destination) {
        (IpAddr::V4(source), IpAddr::V4(destination)) => {
            let Ok(length) = packet_wire::icmpv4::emit_error(
                &mut bytes[20..],
                kind,
                code,
                parameter,
                quote,
                &[],
            ) else {
                return;
            };
            bytes.truncate(20 + length);
            packet_wire::ipv4::Ipv4Header {
                source,
                destination,
                protocol: 1,
                hop_limit: 64,
                dscp_ecn: 0,
                identification: 0,
                fragment: 0,
                options: &[],
            }
            .emit(&mut bytes)
        }
        (IpAddr::V6(source), IpAddr::V6(destination)) => {
            let Ok(length) = packet_wire::icmpv6::emit_error(
                &mut bytes[40..],
                kind,
                code,
                parameter,
                (source, destination),
                quote,
                &[],
            ) else {
                return;
            };
            bytes.truncate(40 + length);
            packet_wire::ipv6::Ipv6Header {
                source,
                destination,
                next_header: 58,
                hop_limit: 64,
                traffic_class: 0,
                flow_label: 0,
            }
            .emit(&mut bytes)
        }
        _ => return,
    };
    if result.is_ok() {
        let use_link_hop_limit = bytes[0] >> 4 == 6;
        replies.enqueue_packet(bytes, problem.interface, (bound, 0), now, use_link_hop_limit);
    }
}
