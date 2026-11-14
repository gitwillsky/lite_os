//! 在原生 actor 内借用真实接收、选路和发送状态，生成只读管理回复。

use super::NeighborState;
use ::bgp::{RoutingDomain, State};
use alloc::{format, string::ToString, vec::Vec};
use native_runtime::native_abi::{self, Status};
use router_protocol::bgp::{AuthenticationStatus, NeighborStatus, Snapshot};

pub(super) fn observe_routes(
    peers: &[NeighborState],
    routes: &RoutingDomain,
    pending: bool,
    query: router_protocol::tracking::IpPrefix,
) -> Result<router_protocol::bgp::Routes, Status> {
    use router_protocol::bgp::{RouteDecision, RouteObservation, Routes, Selected};
    let prefix = packet_wire::bgp::nlri::Prefix {
        address: query.network,
        length: query.length,
    };
    let mut received = Vec::new();
    received
        .try_reserve_exact(peers.len())
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    for peer in peers {
        for connection in core::iter::once(&peer.outgoing).chain(peer.incoming.iter()) {
            let Some(routes) = connection
                .transport
                .as_ref()
                .and_then(|transport| routes.peer_routes(transport.key))
            else {
                continue;
            };
            for (path_id, route) in routes.paths(prefix) {
                received
                    .try_reserve(1)
                    .map_err(|_| native_abi::ERR_NO_MEMORY)?;
                received.push(RouteObservation {
                    neighbor: peer.name.as_str().to_string(),
                    address: peer.parameters.address,
                    path_id,
                    decision: if pending {
                        RouteDecision::Pending
                    } else {
                        route.decision()
                    },
                    next_hop: route.next_hop().address,
                    attributes: route
                        .attributes()
                        .observe()
                        .map_err(|_| native_abi::ERR_NO_MEMORY)?,
                });
            }
        }
    }
    let mut exported = Vec::new();
    exported
        .try_reserve_exact(peers.len())
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    for peer in peers {
        let paths = core::iter::once(&peer.outgoing)
            .chain(peer.incoming.iter())
            .filter_map(|connection| connection.output.as_ref())
            .flat_map(|(_, output)| output.advertised_paths(prefix));
        let mut advertised = false;
        let mut path_ids = Vec::new();
        for path in paths {
            advertised = true;
            if let Some(id) = path {
                path_ids
                    .try_reserve(1)
                    .map_err(|_| native_abi::ERR_NO_MEMORY)?;
                path_ids.push(id);
            }
        }
        exported.push(router_protocol::bgp::ExportObservation {
            neighbor: peer.name.as_str().to_string(),
            address: peer.parameters.address,
            advertised,
            path_ids,
        });
    }
    let selected = routes
        .export_route(prefix)
        .map(|route| {
            Ok::<_, Status>(Selected {
                local: route.peer.is_none(),
                next_hop: route.next_hop.address,
                preference: route.preference,
                attributes: route
                    .attributes
                    .observe()
                    .map_err(|_| native_abi::ERR_NO_MEMORY)?,
            })
        })
        .transpose()?;
    Ok(Routes {
        query,
        pending,
        received,
        exported,
        originated: routes.network_is_available(prefix),
        redistributed: routes.redistributed_route(prefix),
        selected,
    })
}
pub(super) fn snapshot(
    peers: &[NeighborState],
    routes: &RoutingDomain,
    pending: bool,
) -> Result<Snapshot, Status> {
    let mut neighbors = Vec::new();
    neighbors
        .try_reserve_exact(peers.len())
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    for peer in peers {
        let connection = core::iter::once(&peer.outgoing)
            .chain(peer.incoming.iter())
            .max_by_key(|connection| match connection.session.state() {
                State::Idle => 0,
                State::Active => 1,
                State::Connect => 2,
                State::OpenSent => 3,
                State::OpenConfirm => 4,
                State::Established => 5,
            })
            .expect("outgoing session always exists");
        let negotiated = (connection.session.state() == State::Established)
            .then(|| connection.session.negotiated())
            .flatten();
        neighbors.push(NeighborStatus {
            name: peer.name.as_str().to_string(),
            address: peer.parameters.address,
            interface: peer.parameters.interface.clone(),
            state: format!("{:?}", connection.session.state()),
            shutdown: peer.parameters.shutdown,
            received_routes: connection
                .transport
                .as_ref()
                .and_then(|transport| routes.peer_routes(transport.key))
                .map_or(0, |routes| routes.len()),
            import_policy_configured: peer.parameters.import.is_some(),
            export_policy_configured: peer.parameters.export.is_some(),
            authentication: if peer.parameters.tcp_ao.is_some() {
                let selection = connection
                    .transport
                    .as_ref()
                    .and_then(|transport| transport.socket.ao_status().ok())
                    .map_or_else(Default::default, |status| status.selection);
                AuthenticationStatus::Ao {
                    send_id: selection.current,
                    receive_id: selection.receive,
                }
            } else if peer.parameters.md5_password.is_some() {
                AuthenticationStatus::Md5
            } else {
                AuthenticationStatus::None
            },
            route_reflector_client: peer.parameters.route_reflector_client,
            ipv4_unicast: negotiated.is_some_and(|peer| peer.ipv4_unicast),
            ipv6_unicast: negotiated.is_some_and(|peer| peer.ipv6_unicast),
            receive_add_path: negotiated
                .map(|peer| peer.receive_add_path)
                .filter(|families| families.iter().any(|enabled| *enabled)),
            send_add_path: negotiated
                .map(|peer| peer.send_add_path)
                .filter(|families| families.iter().any(|enabled| *enabled)),
            retry_blocked: !peer.parameters.shutdown && peer.open_offer.is_none(),
            advertised_routes: connection
                .output
                .as_ref()
                .map_or(0, |(_, output)| output.len()),
            error: if peer.scope.unavailable(&peer.parameters) {
                Some("接口未安装".into())
            } else if connection.session.state() == State::Established {
                connection
                    .output
                    .as_ref()
                    .and_then(|(_, output)| output.rejected_route())
                    .map(|(prefix, reason)| {
                        format!(
                            "route {}/{} cannot be advertised: {}",
                            prefix.address,
                            prefix.length,
                            if reason == packet_wire::PacketError::Unsupported {
                                "next hop or confederation AS cannot be represented by this peer"
                            } else {
                                "peer message limit exceeded"
                            }
                        )
                    })
            } else {
                peer.error.clone()
            },
        });
    }
    Ok(Snapshot {
        cluster_id: routes.cluster_id(),
        neighbors,
        selected_routes: routes.selected().len(),
        originated_routes: routes.originated_len(),
        redistributed_routes: routes.redistributed_len(),
        routing_pending: pending,
    })
}
