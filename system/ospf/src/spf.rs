//! RFC 2328 §16.1 与 RFC 5340 §4.8 的区域内 SPF。只从已建立的邻接引出本机第一跳。

use super::database::Database;
use alloc::{collections::BinaryHeap, rc::Rc, vec::Vec};
use core::{
    cmp::{Ordering, Reverse},
    net::{IpAddr, Ipv4Addr},
};
use fallible_tree::FallibleMap;
use native_runtime::native_abi::{self, Status};
use packet_wire::ospf::{
    InterAreaPrefixLsa, IntraAreaPrefixLsa, LS_INFINITY, Lsa, NetworkLsa, NetworkParameters,
    RouterLsaV2, RouterLsaV3, Version,
};
use router_protocol::{
    ospf::PREFERENCE,
    routing::{Action, Attributes, Batch, EncodeError, Family, Member, NextHop, Route, Target},
    tracking::IpPrefix,
};

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Vertex {
    // 同代价时先展开网络节点，确保零代价网络边上的等价第一跳在路由器出队前合并。
    Network(u32, u32),
    Router(u32),
}

#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
struct Edge {
    destination: Vertex,
    local: u32,
    remote: u32,
    cost: u16,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) struct Neighbor {
    pub(super) area: u32,
    pub(super) router: u32,
    pub(super) interface: u32,
    pub(super) remote_interface: u32,
    pub(super) gateway: IpAddr,
    pub(super) cost: u16,
}

#[derive(Clone)]
struct Path {
    cost: u32,
    members: Rc<Vec<Member>>,
}

#[derive(Default)]
pub(super) struct Areas {
    pub(super) attached: Vec<u32>,
    pub(super) multiple: bool,
    pub(super) border: bool,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) struct Summary {
    pub(super) area: u32,
    pub(super) prefix: IpPrefix,
    pub(super) metric: u32,
}

pub(super) struct Selection {
    pub(super) batch: Vec<u8>,
    pub(super) summaries: Vec<Summary>,
}

struct SelectedPath {
    path: Path,
    area: u32,
    inter: bool,
}

impl Path {
    fn merge(&mut self, other: &Self) -> Result<Ordering, Status> {
        let ordering = other.cost.cmp(&self.cost);
        if ordering == Ordering::Less {
            *self = other.clone();
        } else if ordering == Ordering::Equal
            && !Rc::ptr_eq(&self.members, &other.members)
            && other
                .members
                .iter()
                .any(|member| self.members.binary_search(member).is_err())
        {
            let mut members = Vec::new();
            members
                .try_reserve_exact(self.members.len() + other.members.len())
                .map_err(|_| native_abi::ERR_NO_MEMORY)?;
            members.extend_from_slice(&self.members);
            members.extend_from_slice(&other.members);
            members.sort_unstable();
            members.dedup();
            self.members = Rc::try_new(members).map_err(|_| native_abi::ERR_NO_MEMORY)?;
        }
        Ok(ordering)
    }
}

type Paths = FallibleMap<Vertex, Path>;
type Prefixes = FallibleMap<IpPrefix, SelectedPath>;

#[derive(Default)]
struct Graph {
    edges: FallibleMap<Vertex, Vec<Edge>>,
    prefixes: Vec<(Vertex, IpPrefix, u16)>,
    router_options: FallibleMap<u32, u32>,
}

impl Graph {
    fn edge(&mut self, source: Vertex, edge: Edge) -> Result<(), Status> {
        if let Some(edges) = self.edges.get_mut(&source) {
            edges
                .try_reserve(1)
                .map_err(|_| native_abi::ERR_NO_MEMORY)?;
            edges.push(edge);
        } else {
            let mut edges = Vec::new();
            edges
                .try_reserve_exact(1)
                .map_err(|_| native_abi::ERR_NO_MEMORY)?;
            edges.push(edge);
            self.edges
                .try_insert(source, edges)
                .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        }
        Ok(())
    }

    fn prefix(&mut self, owner: Vertex, prefix: IpPrefix, cost: u16) -> Result<(), Status> {
        if !usable_prefix(prefix) {
            return Ok(());
        }
        self.prefixes
            .try_reserve(1)
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        self.prefixes.push((owner, prefix, cost));
        Ok(())
    }

    fn add_v2(&mut self, lsa: Lsa<'_>) -> Result<(), Status> {
        let key = lsa.header().fields().key;
        if key.kind == 1
            && key.link_state_id == key.advertising_router
            && let Ok(body) = RouterLsaV2::parse(lsa)
        {
            let source = Vertex::Router(key.advertising_router);
            self.router_options
                .try_insert(key.advertising_router, u32::from(body.flags) << 24)
                .map_err(|_| native_abi::ERR_NO_MEMORY)?;
            for link in body.links() {
                let target = match link.kind {
                    1 => Vertex::Router(link.link_id),
                    2 => Vertex::Network(0, link.link_id),
                    3 => {
                        if let Some(prefix) = ipv4_prefix(link.link_id, link.link_data) {
                            self.prefix(source, prefix, link.cost)?;
                        }
                        continue;
                    }
                    _ => continue,
                };
                if link.cost != 0 {
                    self.edge(
                        source,
                        Edge {
                            destination: target,
                            local: 0,
                            remote: 0,
                            cost: link.cost,
                        },
                    )?;
                }
            }
        } else if let Ok(body) = NetworkLsa::parse(lsa)
            && let NetworkParameters::V2 { mask } = body.parameters
        {
            let source = Vertex::Network(0, key.link_state_id);
            self.network(source, body)?;
            if let Some(prefix) = ipv4_prefix(key.link_state_id, mask.into()) {
                self.prefix(source, prefix, 0)?;
            }
        }
        Ok(())
    }

    fn add_v3(&mut self, lsa: Lsa<'_>) -> Result<(), Status> {
        let key = lsa.header().fields().key;
        match key.kind {
            0x2001 => {
                let Ok(body) = RouterLsaV3::parse(lsa) else {
                    return Ok(());
                };
                // 区域记录按 LS ID 升序。多个 Router-LSA 的能力取 LS ID 最小的一份，链路合并。
                let options =
                    if let Some(&options) = self.router_options.get(&key.advertising_router) {
                        options
                    } else {
                        let options = u32::from(body.flags) << 24 | body.options;
                        self.router_options
                            .try_insert(key.advertising_router, options)
                            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
                        options
                    };
                if options & 0x11 != 0x11 {
                    return Ok(());
                }
                let source = Vertex::Router(key.advertising_router);
                for link in body.links() {
                    let target = match link.kind {
                        1 => Vertex::Router(link.neighbor_router),
                        2 => Vertex::Network(link.neighbor_router, link.neighbor_interface),
                        _ => continue,
                    };
                    if link.cost != 0 {
                        self.edge(
                            source,
                            Edge {
                                destination: target,
                                local: link.interface,
                                remote: link.neighbor_interface,
                                cost: link.cost,
                            },
                        )?;
                    }
                }
            }
            0x2002 => {
                if let Ok(body) = NetworkLsa::parse(lsa) {
                    self.network(
                        Vertex::Network(key.advertising_router, key.link_state_id),
                        body,
                    )?;
                }
            }
            0x2009 => {
                if let Ok(body) = IntraAreaPrefixLsa::parse(lsa) {
                    if body.reference.advertising_router != key.advertising_router {
                        return Ok(());
                    }
                    let owner = match body.reference.kind {
                        0x2001 if body.reference.link_state_id == 0 => {
                            Vertex::Router(body.reference.advertising_router)
                        }
                        0x2002 => Vertex::Network(
                            body.reference.advertising_router,
                            body.reference.link_state_id,
                        ),
                        _ => return Ok(()),
                    };
                    for (prefix, cost) in body
                        .prefixes()
                        .filter(|(prefix, _)| prefix.options & 1 == 0)
                    {
                        self.prefix(
                            owner,
                            IpPrefix {
                                network: prefix.network.into(),
                                length: prefix.length,
                            },
                            cost,
                        )?;
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn network(&mut self, source: Vertex, body: NetworkLsa<'_>) -> Result<(), Status> {
        for router in body.routers() {
            self.edge(
                source,
                Edge {
                    destination: Vertex::Router(router),
                    local: 0,
                    remote: 0,
                    cost: 0,
                },
            )?;
        }
        Ok(())
    }

    fn border(&self, router: u32) -> bool {
        self.router_options
            .get(&router)
            .is_some_and(|options| options & (1 << 24) != 0)
    }

    fn has_backlink(&self, source: Vertex, edge: Edge) -> bool {
        self.edges.get(&edge.destination).is_some_and(|edges| {
            let start = edges.partition_point(|back| back.destination < source);
            edges[start..]
                .iter()
                .take_while(|back| back.destination == source)
                .any(|back| {
                    !matches!(
                        (source, edge.destination),
                        (Vertex::Router(_), Vertex::Router(_))
                    ) || (edge.local == back.remote && edge.remote == back.local)
                })
        })
    }

    fn shortest(
        &mut self,
        router: u32,
        neighbors: &[Neighbor],
        version: Version,
    ) -> Result<Paths, Status> {
        let root = Vertex::Router(router);
        self.edges.for_each_mut(|_, edges| edges.sort_unstable());
        let mut paths = Paths::default();
        let mut queue = BinaryHeap::new();
        for neighbor in neighbors {
            let (local, remote) = if version == Version::V2 {
                (0, 0)
            } else {
                (neighbor.interface, neighbor.remote_interface)
            };
            let edge = Edge {
                destination: Vertex::Router(neighbor.router),
                local,
                remote,
                cost: neighbor.cost,
            };
            if !self.has_backlink(root, edge) {
                continue;
            }
            let mut members = Vec::new();
            members
                .try_reserve_exact(1)
                .map_err(|_| native_abi::ERR_NO_MEMORY)?;
            members.push(Member {
                target: Target::Direct {
                    interface_index: neighbor.interface,
                    gateway: Some(neighbor.gateway),
                    on_link: true,
                },
                weight: 1,
            });
            let path = Path {
                cost: u32::from(neighbor.cost),
                members: Rc::try_new(members).map_err(|_| native_abi::ERR_NO_MEMORY)?,
            };
            improve(&mut paths, &mut queue, edge.destination, path)?;
        }
        while let Some(Reverse((cost, node))) = queue.pop() {
            let path = paths.get(&node).expect("queued SPF vertex");
            if path.cost != cost {
                continue;
            }
            let path = path.clone();
            for &edge in self.edges.get(&node).into_iter().flatten() {
                if edge.destination != root
                    && self.has_backlink(node, edge)
                    && let Some(cost) = cost.checked_add(u32::from(edge.cost))
                {
                    improve(
                        &mut paths,
                        &mut queue,
                        edge.destination,
                        Path {
                            cost,
                            members: path.members.clone(),
                        },
                    )?;
                }
            }
        }
        Ok(paths)
    }
}

fn improve(
    paths: &mut Paths,
    queue: &mut BinaryHeap<Reverse<(u32, Vertex)>>,
    node: Vertex,
    path: Path,
) -> Result<(), Status> {
    let cost = path.cost;
    if let Some(known) = paths.get_mut(&node) {
        if known.merge(&path)? != Ordering::Less {
            return Ok(());
        }
    } else {
        paths
            .try_insert(node, path)
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    }
    queue
        .try_reserve(1)
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    queue.push(Reverse((cost, node)));
    Ok(())
}

fn ipv4_prefix(address: u32, mask: u32) -> Option<IpPrefix> {
    let length = mask.leading_ones() as u8;
    (length == mask.count_ones() as u8).then_some(IpPrefix {
        network: Ipv4Addr::from(address & mask).into(),
        length,
    })
}

fn usable_prefix(prefix: IpPrefix) -> bool {
    !prefix.network.is_multicast()
        && !prefix.network.is_loopback()
        && !matches!(prefix.network, IpAddr::V6(address) if address.is_unicast_link_local() || address.to_ipv4_mapped().is_some())
}

pub(super) fn select(
    database: &Database,
    router: u32,
    version: Version,
    roots: &[Neighbor],
    areas: &Areas,
    now: i64,
) -> Result<Selection, Status> {
    let mut prefixes = Prefixes::default();
    let local = Rc::try_new(Vec::new()).map_err(|_| native_abi::ERR_NO_MEMORY)?;
    debug_assert!(roots.is_sorted_by_key(|root| root.area));
    for &area in &areas.attached {
        let start = roots.partition_point(|root| root.area < area);
        let count = roots[start..].partition_point(|root| root.area == area);
        let mut graph = Graph::default();
        for lsa in database.area_lsas(version, area, now) {
            if count == 0 && lsa.header().fields().key.advertising_router != router {
                continue;
            }
            match version {
                Version::V2 => graph.add_v2(lsa)?,
                Version::V3 => graph.add_v3(lsa)?,
            }
        }
        let paths = graph.shortest(router, &roots[start..start + count], version)?;
        for &(node, prefix, cost) in &graph.prefixes {
            let path = if node == Vertex::Router(router) {
                Some(Path {
                    cost: u32::from(cost),
                    members: local.clone(),
                })
            } else {
                extend_path(&paths, node, u32::from(cost))
            };
            if let Some(path) = path {
                select_path(
                    &mut prefixes,
                    prefix,
                    SelectedPath {
                        path,
                        area,
                        inter: false,
                    },
                )?;
            }
        }
        // §16.2：多区域路由器只接收骨干区摘要。只把可达的边界路由器作为来源。
        if count != 0 && (!areas.multiple || area == 0) {
            for lsa in database.summaries(version, area, now) {
                let abr = lsa.header().fields().key.advertising_router;
                if abr == router || !graph.border(abr) {
                    continue;
                }
                let Ok(summary) = InterAreaPrefixLsa::parse(lsa) else {
                    continue;
                };
                let prefix = IpPrefix {
                    network: summary.network,
                    length: summary.length,
                };
                if summary.metric < LS_INFINITY
                    && usable_prefix(prefix)
                    && let Some(path) = extend_path(&paths, Vertex::Router(abr), summary.metric)
                {
                    select_path(
                        &mut prefixes,
                        prefix,
                        SelectedPath {
                            path,
                            area,
                            inter: true,
                        },
                    )?;
                }
            }
        }
    }
    let mut summaries = Vec::new();
    if areas.border {
        for &target in &areas.attached {
            for (&prefix, route) in prefixes.iter() {
                if target == route.area
                    || (target == 0 && route.inter)
                    || route.path.cost >= LS_INFINITY
                {
                    continue;
                }
                summaries
                    .try_reserve(1)
                    .map_err(|_| native_abi::ERR_NO_MEMORY)?;
                summaries.push(Summary {
                    area: target,
                    prefix,
                    metric: route.path.cost,
                });
            }
        }
    }
    Ok(Selection {
        batch: encode_routes(&prefixes, version)?,
        summaries,
    })
}

fn extend_path(paths: &Paths, node: Vertex, cost: u32) -> Option<Path> {
    let path = paths.get(&node)?;
    Some(Path {
        cost: path.cost.checked_add(cost)?,
        members: path.members.clone(),
    })
}

fn select_path(
    prefixes: &mut Prefixes,
    prefix: IpPrefix,
    next: SelectedPath,
) -> Result<(), Status> {
    if let Some(previous) = prefixes.get_mut(&prefix) {
        // 区域内优先。等价第一跳只在关联区域内合并，避免把下一跳通告回它所属的区域。
        let ordering = next
            .inter
            .cmp(&previous.inter)
            .then_with(|| {
                previous
                    .path
                    .members
                    .is_empty()
                    .cmp(&next.path.members.is_empty())
            })
            .then_with(|| next.path.cost.cmp(&previous.path.cost))
            .then_with(|| next.area.cmp(&previous.area));
        match ordering {
            Ordering::Less => *previous = next,
            Ordering::Equal => {
                previous.path.merge(&next.path)?;
            }
            Ordering::Greater => {}
        }
    } else {
        prefixes
            .try_insert(prefix, next)
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    }
    Ok(())
}

fn encode_routes(prefixes: &Prefixes, version: Version) -> Result<Vec<u8>, Status> {
    let mut metrics = FallibleMap::default();
    let mut groups = FallibleMap::default();
    let mut attributes = Vec::new();
    let mut hops = Vec::new();
    let mut routes = Vec::new();
    routes
        .try_reserve_exact(prefixes.len())
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    for (&prefix, route) in prefixes.iter() {
        let path = &route.path;
        if path.members.is_empty() {
            continue;
        }
        let metric = match metrics.get(&path.cost) {
            Some(&index) => index,
            None => {
                let index = attributes.len() as u32;
                metrics
                    .try_insert(path.cost, index)
                    .map_err(|_| native_abi::ERR_NO_MEMORY)?;
                attributes
                    .try_reserve(1)
                    .map_err(|_| native_abi::ERR_NO_MEMORY)?;
                attributes.push(Attributes {
                    preference: PREFERENCE,
                    metric: path.cost,
                });
                index
            }
        };
        let hop = match groups.get(&path.members) {
            Some(&index) => index,
            None => {
                let index = hops.len() as u32;
                groups
                    .try_insert(path.members.clone(), index)
                    .map_err(|_| native_abi::ERR_NO_MEMORY)?;
                hops.try_reserve(1).map_err(|_| native_abi::ERR_NO_MEMORY)?;
                hops.push(NextHop::new(Action::Forward, &path.members));
                index
            }
        };
        routes.push(Route {
            network: prefix.network,
            length: prefix.length,
            attributes: metric,
            next_hop: hop,
        });
    }
    let family = if version == Version::V2 {
        Family::Ipv4
    } else {
        Family::Ipv6
    };
    Batch::encode(family, &attributes, &hops, &routes).map_err(|error| match error {
        EncodeError::NoMemory => native_abi::ERR_NO_MEMORY,
        EncodeError::Invalid => native_abi::ERR_BAD_STATE,
    })
}
