//! 预路由按源网段、入接口和目的索引分类；规则次序独立于前缀长度。

mod address_sets;
mod policy;

use super::SocketError;
use address_sets::{AddressSets, SetIndex};
use alloc::{string::String, vec::Vec};
use core::{net::IpAddr, ops::Range};
use native_runtime::native_abi::{self, Status};
use prefix_index::{Address, Poptrie};
use router_protocol::firewall::{
    Chain, ConnectionState, Destination, Ingress, MAX_JUMP_DEPTH, NatSource, NatTarget, Prerouting,
    PreroutingAction, TransportMatch,
};

pub(crate) use policy::counter_page;
pub(super) use policy::{Firewall, PolicySnapshot};

type Selector = (Option<ConnectionState>, u32, Option<TransportMatch>);

/// 已选中的转换动作。自动转换的目标在选路后确定。
pub(super) struct NatAction {
    pub(super) source: Option<NatSource>,
    pub(super) destination: Option<NatTarget>,
}

/// 区分服务匹配与连接跟踪。单纯按端口过滤不建立流表项。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct Inspection {
    pub(super) transport: bool,
    pub(super) connections: bool,
    // 本机地址的报文只在有目的转换规则时试判，纯过滤规则不接管本机交付。
    pub(super) destination_nat: bool,
}

impl Inspection {
    pub(super) fn required(self) -> bool {
        self.transport || self.connections
    }

    fn merge(self, other: Self) -> Self {
        Self {
            transport: self.transport || other.transport,
            connections: self.connections || other.connections,
            destination_nat: self.destination_nat || other.destination_nat,
        }
    }
}

#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
struct Mark {
    order: usize,
    value: u32,
}

#[derive(Clone, Copy)]
struct Control {
    order: usize,
    action: PreroutingAction,
}

struct Rule<A> {
    source: (A, u8),
    selector: Selector,
    network: A,
    length: u8,
    order: usize,
    action: PreroutingAction,
}

struct PrefixActions {
    marks: Range<usize>,
    controls: Range<usize>,
    has_marks: bool,
    first_control: Option<Control>,
}

struct InterfaceRules<A> {
    selector: Selector,
    destinations: Poptrie<A>,
    actions: Vec<PrefixActions>,
    marks: Vec<Mark>,
    controls: Vec<Control>,
}

impl<A: Address> InterfaceRules<A> {
    fn matching(
        &self,
        destination: A,
    ) -> Option<(&Self, usize, impl Iterator<Item = usize> + Clone + '_)> {
        let mut matching = self.destinations.matching_indices(destination);
        let index = matching.next()?;
        Some((self, index, matching))
    }

    fn marks_in<'a>(
        &'a self,
        matching: impl Iterator<Item = usize> + 'a,
        orders: Range<usize>,
    ) -> impl Iterator<Item = Mark> + 'a {
        matching.flat_map(move |index| {
            marks_in(
                &self.marks[self.actions[index].marks.clone()],
                orders.clone(),
            )
        })
    }

    fn next_control(
        &self,
        index: usize,
        parents: impl Iterator<Item = usize>,
        start: usize,
    ) -> Option<Control> {
        let first = self.actions[index].first_control?;
        if first.order >= start {
            return Some(first);
        }
        // 返回后要越过已处理的跳转。最早候选仍只来自本次命中的前缀和祖先。
        core::iter::once(index)
            .chain(parents)
            .filter_map(|index| {
                next_control(&self.controls[self.actions[index].controls.clone()], start)
            })
            .min_by_key(|entry| entry.order)
    }
}

struct SetRule<A> {
    source: (A, u8),
    key: (u32, Selector),
    order: usize,
    action: PreroutingAction,
}

#[derive(Default)]
struct SetRules {
    entries: Vec<((u32, Selector), PrefixActions)>,
    marks: Vec<Mark>,
    controls: Vec<Control>,
}

impl SetRules {
    fn compile<A>(rules: &mut [SetRule<A>]) -> Result<Self, SocketError> {
        rules.sort_unstable_by_key(|rule| (rule.key, rule.order));
        let mut result = Self::default();
        for group in rules.chunk_by(|left, right| left.key == right.key) {
            let actions = compile_actions(
                group.iter().map(|rule| (rule.order, rule.action)),
                &mut result.marks,
                &mut result.controls,
            )?;
            result
                .entries
                .try_reserve(1)
                .map_err(|_| SocketError::NoMemory)?;
            result.entries.push((group[0].key, actions));
        }
        Ok(result)
    }

    fn matching(&self, set: u32, selector: Selector) -> Option<&PrefixActions> {
        let index = self
            .entries
            .binary_search_by_key(&(set, selector), |entry| entry.0)
            .ok()?;
        Some(&self.entries[index].1)
    }
}

struct CompiledChain {
    name: Option<String>,
    rules: Range<usize>,
    ipv4: FamilyRules<u32>,
    ipv6: FamilyRules<u128>,
}

struct SourceRules<A> {
    interfaces: Vec<InterfaceRules<A>>,
    sets: SetRules,
}

struct FamilyRules<A> {
    sources: Poptrie<A>,
    groups: Vec<SourceRules<A>>,
    // 该族没有任何可执行的赋标动作时，找到控制动作即可结束，无需再次遍历匹配索引。
    has_marks: bool,
}

impl<A: Address> FamilyRules<A> {
    fn compile(
        rules: &[router_protocol::firewall::Rule],
        set_inspection: &mut [[Option<Inspection>; 2]],
        family: usize,
        start: usize,
        address: impl Fn(IpAddr) -> Option<A>,
    ) -> Result<(Self, Inspection), SocketError> {
        let prefix = |prefix: Option<router_protocol::tracking::IpPrefix>| match prefix {
            None => Some((A::zero(), 0)),
            Some(prefix) => Some((address(prefix.network)?, prefix.length)),
        };
        let mut destinations = Vec::new();
        let mut sets = Vec::new();
        let mut inspection = Inspection::default();
        for (offset, rule) in rules.iter().enumerate() {
            let ingress = match rule.ingress {
                Ingress::Any => 0,
                Ingress::Interface(index) => index,
                Ingress::Unavailable => continue,
            };
            let Some(source) = prefix(rule.source) else {
                continue;
            };
            let selector = (rule.connection_state, ingress, rule.transport);
            let order = start + offset;
            let action = rule.action;
            let target = match action {
                PreroutingAction::DestinationNat { address, .. } => Some(address),
                PreroutingAction::SourceNat(target) => Some(target.address),
                _ => None,
            };
            if target.is_some_and(|target| address(target).is_none()) {
                continue;
            }
            let needs = Inspection {
                transport: rule.transport.is_some(),
                connections: target.is_some()
                    || matches!(action, PreroutingAction::Masquerade)
                    || rule.connection_state.is_some(),
                destination_nat: matches!(action, PreroutingAction::DestinationNat { .. }),
            };
            let destination = match rule.destination {
                Destination::Any => prefix(None),
                Destination::Prefix(destination) => prefix(Some(destination)),
                Destination::Set(index) => {
                    let tracking = &mut set_inspection[index as usize][family];
                    *tracking = Some(tracking.unwrap_or_default().merge(needs));
                    sets.try_reserve(1).map_err(|_| SocketError::NoMemory)?;
                    sets.push(SetRule {
                        source,
                        key: (index, selector),
                        order,
                        action,
                    });
                    continue;
                }
            };
            if let Some((network, length)) = destination {
                inspection = inspection.merge(needs);
                destinations
                    .try_reserve(1)
                    .map_err(|_| SocketError::NoMemory)?;
                destinations.push(Rule {
                    source,
                    selector,
                    network,
                    length,
                    order,
                    action,
                });
            }
        }
        let has_marks = destinations
            .iter()
            .any(|rule| matches!(rule.action, PreroutingAction::SetMark { .. }))
            || sets
                .iter()
                .any(|rule| matches!(rule.action, PreroutingAction::SetMark { .. }));
        destinations.sort_unstable_by_key(|rule| rule.source);
        sets.sort_unstable_by_key(|rule| rule.source);
        let sources = Poptrie::build(
            destinations
                .iter()
                .map(|rule| rule.source)
                .chain(sets.iter().map(|rule| rule.source)),
        )
        .map_err(prefix_error)?;
        let mut groups = Vec::new();
        groups
            .try_reserve_exact(sources.prefixes().len())
            .map_err(|_| SocketError::NoMemory)?;
        let mut destinations = destinations.as_mut_slice();
        let mut sets = sets.as_mut_slice();
        // 每个源网段只编译所属规则。报文命中时合并各匹配源网段的候选，按全链顺序裁决。
        for prefix in sources.prefixes() {
            let source = (prefix.network, prefix.length);
            let direct_count = destinations.partition_point(|rule| rule.source == source);
            let set_count = sets.partition_point(|rule| rule.source == source);
            let (direct, remaining) = destinations.split_at_mut(direct_count);
            destinations = remaining;
            let (members, remaining) = sets.split_at_mut(set_count);
            sets = remaining;
            groups.push(SourceRules {
                interfaces: compile(direct)?,
                sets: SetRules::compile(members)?,
            });
        }
        Ok((
            Self {
                sources,
                groups,
                has_marks,
            },
            inspection,
        ))
    }

    fn matching(&self, source: A) -> impl Iterator<Item = &SourceRules<A>> + Clone + '_ {
        self.sources
            .matching_indices(source)
            .map(|index| &self.groups[index])
    }
}

/// 不可变规则定义和决策索引。集合成员变化时复用同一份规则与名字。
pub(super) struct Classification {
    chains: Vec<CompiledChain>,
    rules: Vec<router_protocol::firewall::Rule>,
    set_names: Vec<String>,
    inspection: [Inspection; 2],
    // 每族 None 表示无可达规则引用此集合，该族不安装成员索引。
    set_inspection: Vec<[Option<Inspection>; 2]>,
}

impl Classification {
    fn matches(&self, chain: Prerouting<'_>) -> bool {
        self.rules.iter().copied().eq(chain.rules())
            && self
                .set_names
                .iter()
                .map(String::as_str)
                .eq(chain.address_sets().map(|set| set.name))
            && self
                .chains
                .iter()
                .map(|chain| (chain.name.as_deref(), chain.rules.len()))
                .eq(chain
                    .chains()
                    .map(|chain| (chain.name, chain.rules().len())))
    }

    fn same_rules(&self, other: &Self) -> bool {
        self.rules == other.rules
            && self.set_names == other.set_names
            && self
                .chains
                .iter()
                .map(|chain| (chain.name.as_deref(), &chain.rules))
                .eq(other
                    .chains
                    .iter()
                    .map(|chain| (chain.name.as_deref(), &chain.rules)))
    }

    fn compile(program: Prerouting<'_>) -> Result<Self, SocketError> {
        use router_protocol::routing::EncodeError;
        program.validate_jumps().map_err(|error| match error {
            EncodeError::Invalid => SocketError::Invalid,
            EncodeError::NoMemory => SocketError::NoMemory,
        })?;
        let reachable = reachable_chains(program)?;
        let mut chains = Vec::new();
        let mut rules = Vec::new();
        let mut set_names = Vec::new();
        let mut set_inspection = Vec::new();
        let set_count = program.address_sets().len();
        chains
            .try_reserve_exact(program.chains().len())
            .map_err(|_| SocketError::NoMemory)?;
        rules
            .try_reserve_exact(program.rules().len())
            .map_err(|_| SocketError::NoMemory)?;
        set_names
            .try_reserve_exact(set_count)
            .map_err(|_| SocketError::NoMemory)?;
        set_inspection
            .try_reserve_exact(set_count)
            .map_err(|_| SocketError::NoMemory)?;
        set_inspection.resize(set_count, [None; 2]);
        for set in program.address_sets() {
            set_names.push(capture_name(set.name).map_err(|_| SocketError::NoMemory)?);
        }
        let mut inspection = [Inspection::default(); 2];
        for (index, chain) in program.chains().enumerate() {
            let active = reachable.get(index).copied().unwrap_or(true);
            let start = rules.len();
            rules.extend(chain.rules());
            let installed = if active { &rules[start..] } else { &[] };
            let (ipv4, tracks_ipv4) = FamilyRules::compile(installed, &mut set_inspection, 0, start, |address| {
                if let IpAddr::V4(address) = address { Some(u32::from(address)) } else { None }
            })?;
            let (ipv6, tracks_ipv6) = FamilyRules::compile(installed, &mut set_inspection, 1, start, |address| {
                if let IpAddr::V6(address) = address { Some(u128::from(address)) } else { None }
            })?;
            inspection[0] = inspection[0].merge(tracks_ipv4);
            inspection[1] = inspection[1].merge(tracks_ipv6);
            chains.push(CompiledChain {
                name: chain
                    .name
                    .map(capture_name)
                    .transpose()
                    .map_err(|_| SocketError::NoMemory)?,
                rules: start..rules.len(),
                ipv4,
                ipv6,
            });
        }
        Ok(Self {
            chains,
            rules,
            set_names,
            inspection,
            set_inspection,
        })
    }
}

fn reachable_chains(program: Prerouting<'_>) -> Result<Vec<bool>, SocketError> {
    fn mark(id: usize, chains: &[Chain<'_>], reachable: &mut [bool]) {
        if core::mem::replace(&mut reachable[id], true) {
            return;
        }
        for rule in chains[id]
            .rules()
            .filter(|rule| rule.ingress != Ingress::Unavailable)
        {
            if let PreroutingAction::Jump { chain } = rule.action {
                mark(chain as usize, chains, reachable);
            }
        }
    }

    let count = program.chains().len();
    let mut reachable = Vec::new();
    if count > 1 {
        let mut chains = Vec::new();
        chains
            .try_reserve_exact(count)
            .map_err(|_| SocketError::NoMemory)?;
        chains.extend(program.chains());
        reachable
            .try_reserve_exact(count)
            .map_err(|_| SocketError::NoMemory)?;
        reachable.resize(count, false);
        // 调用前已验证深度。设备缺席的跳转不会让未执行的子链开启连接跟踪或分配匹配索引。
        mark(0, &chains, &mut reachable);
    }
    Ok(reachable)
}

fn evaluate(
    rules: &Classification,
    sets: &AddressSets,
    hit: impl FnMut(usize),
    (ingress, (source, destination), classification): PacketMatch<IpAddr>,
) -> Option<(u32, Option<NatAction>)> {
    match (source, destination) {
        (IpAddr::V4(source), IpAddr::V4(destination)) => execute(
            rules,
            &sets.ipv4,
            hit,
            (
                ingress,
                (u32::from(source), u32::from(destination)),
                classification,
            ),
            |chain| &chain.ipv4,
        ),
        (IpAddr::V6(source), IpAddr::V6(destination)) => execute(
            rules,
            &sets.ipv6,
            hit,
            (
                ingress,
                (u128::from(source), u128::from(destination)),
                classification,
            ),
            |chain| &chain.ipv6,
        ),
        _ => None,
    }
}

fn capture_name(name: &str) -> Result<String, Status> {
    let mut copy = String::new();
    copy.try_reserve_exact(name.len())
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    copy.push_str(name);
    Ok(copy)
}

fn prefix_error(error: prefix_index::Error) -> SocketError {
    match error {
        prefix_index::Error::NoMemory => SocketError::NoMemory,
        _ => SocketError::Invalid,
    }
}

fn compile_actions(
    rules: impl Iterator<Item = (usize, PreroutingAction)>,
    marks: &mut Vec<Mark>,
    controls: &mut Vec<Control>,
) -> Result<PrefixActions, SocketError> {
    let mark_start = marks.len();
    let control_start = controls.len();
    for (order, action) in rules {
        if let PreroutingAction::SetMark { value } = action {
            marks.try_reserve(1).map_err(|_| SocketError::NoMemory)?;
            marks.push(Mark { order, value });
        } else {
            controls.try_reserve(1).map_err(|_| SocketError::NoMemory)?;
            controls.push(Control { order, action });
            if !matches!(action, PreroutingAction::Jump { .. }) {
                break;
            }
        }
    }
    Ok(PrefixActions {
        marks: mark_start..marks.len(),
        controls: control_start..controls.len(),
        has_marks: mark_start < marks.len(),
        first_control: controls.get(control_start).copied(),
    })
}

fn compile<A: Address>(rules: &mut [Rule<A>]) -> Result<Vec<InterfaceRules<A>>, SocketError> {
    rules.sort_unstable_by_key(|rule| (rule.selector, rule.network, rule.length, rule.order));
    let mut result = Vec::new();
    for group in rules.chunk_by(|left, right| left.selector == right.selector) {
        let destinations = Poptrie::build(group.iter().map(|rule| (rule.network, rule.length)))
            .map_err(prefix_error)?;
        let prefixes = destinations.prefixes();
        let mut actions = Vec::new();
        actions
            .try_reserve_exact(prefixes.len())
            .map_err(|_| SocketError::NoMemory)?;
        let mut marks = Vec::new();
        let mut controls = Vec::new();
        // 1. 同一前缀按规则次序保存动作。放行、丢弃和返回之后同前缀的规则不可达，跳转可以返回。
        for prefix in prefixes {
            actions.push(compile_actions(
                group[prefix.range()]
                    .iter()
                    .map(|rule| (rule.order, rule.action)),
                &mut marks,
                &mut controls,
            )?);
        }
        // 2. 首次查找直接取覆盖前缀的最早控制动作。只有跳转返回后的续评才定位后续控制动作。
        for (index, prefix) in prefixes.iter().enumerate() {
            let covering = destinations
                .matching_indices(prefix.network)
                .filter(|index| prefixes[*index].length <= prefix.length);
            let first_control = covering
                .clone()
                .filter_map(|index| actions[index].first_control)
                .min_by_key(|entry| entry.order);
            let terminal = covering
                .clone()
                .filter_map(|index| controls[actions[index].controls.clone()].last())
                .filter(|entry| !matches!(entry.action, PreroutingAction::Jump { .. }))
                .map(|entry| entry.order)
                .min()
                .unwrap_or(usize::MAX);
            let has_marks = covering
                .filter_map(|index| marks[actions[index].marks.clone()].first())
                .any(|mark| mark.order < terminal);
            actions[index].first_control = first_control;
            actions[index].has_marks = has_marks;
        }
        result.try_reserve(1).map_err(|_| SocketError::NoMemory)?;
        result.push(InterfaceRules {
            selector: group[0].selector,
            destinations,
            actions,
            marks,
            controls,
        });
    }
    Ok(result)
}

fn next_control(controls: &[Control], start: usize) -> Option<Control> {
    if start == 0 {
        controls.first().copied()
    } else {
        controls
            .get(controls.partition_point(|entry| entry.order < start))
            .copied()
    }
}

fn marks_in(marks: &[Mark], orders: Range<usize>) -> impl Iterator<Item = Mark> + '_ {
    let first = if orders.start == 0 {
        0
    } else {
        marks.partition_point(|mark| mark.order < orders.start)
    };
    marks[first..]
        .iter()
        .copied()
        .take_while(move |mark| mark.order < orders.end)
}

type PacketMatch<A> = (
    u32,
    (A, A),
    (Option<ConnectionState>, Option<TransportMatch>),
);

fn execute<A: Address>(
    program: &Classification,
    set_index: &SetIndex<A>,
    mut hit: impl FnMut(usize),
    packet: PacketMatch<A>,
    family: impl Fn(&CompiledChain) -> &FamilyRules<A>,
) -> Option<(u32, Option<NatAction>)> {
    let mut mark = 0;
    let mut chain_id = 0;
    let mut start = 0;
    // 第一次跳转时才初始化固定栈。配置预备拒绝循环和超过栈容量的嵌套。
    let mut returns: Option<[(usize, usize); MAX_JUMP_DEPTH]> = None;
    let mut depth = 0;
    loop {
        let chain = &program.chains[chain_id];
        let control = classify(
            family(chain),
            set_index,
            &mut hit,
            packet,
            (start, &mut mark),
        );
        match control {
            Some(Control {
                action: PreroutingAction::Accept,
                ..
            }) => return Some((mark, None)),
            Some(Control {
                action:
                    PreroutingAction::DestinationNat {
                        address,
                        port,
                        source,
                    },
                ..
            }) => {
                return Some((
                    mark,
                    Some(NatAction {
                        source,
                        destination: Some(NatTarget { address, port }),
                    }),
                ));
            }
            Some(Control {
                action: PreroutingAction::SourceNat(target),
                ..
            }) => {
                return Some((
                    mark,
                    Some(NatAction {
                        source: Some(NatSource::Fixed(target)),
                        destination: None,
                    }),
                ));
            }
            Some(Control {
                action: PreroutingAction::Masquerade,
                ..
            }) => {
                return Some((
                    mark,
                    Some(NatAction {
                        source: Some(NatSource::Masquerade),
                        destination: None,
                    }),
                ));
            }
            Some(Control {
                action: PreroutingAction::Drop,
                ..
            }) => return None,
            Some(Control {
                action: PreroutingAction::Jump { chain },
                order,
            }) => {
                returns.get_or_insert_default()[depth] = (chain_id, order + 1);
                depth += 1;
                chain_id = chain as usize;
                start = 0;
            }
            Some(Control {
                action: PreroutingAction::Return,
                ..
            })
            | None => {
                if depth == 0 {
                    return Some((mark, None));
                }
                depth -= 1;
                (chain_id, start) = returns.as_ref().expect("validated jump frame")[depth];
            }
            Some(Control {
                action: PreroutingAction::SetMark { .. },
                ..
            }) => {
                unreachable!("mark actions do not affect control flow");
            }
        }
    }
}

fn classify<A: Address>(
    family: &FamilyRules<A>,
    set_index: &SetIndex<A>,
    mut hit: impl FnMut(usize),
    (ingress, (source, destination), (state, transport)): PacketMatch<A>,
    (start, mark): (usize, &mut u32),
) -> Option<Control> {
    let scopes = [
        Some((None, 0)),
        Some((None, ingress)),
        state.map(|state| (Some(state), 0)),
        state.map(|state| (Some(state), ingress)),
    ];
    let services = [
        None,
        transport.map(|service| TransportMatch {
            destination_port: None,
            ..service
        }),
        transport,
    ];
    let service_count = if transport.is_some() { 3 } else { 1 };
    let selectors = move || {
        scopes
            .into_iter()
            .flatten()
            .flat_map(move |(state, ingress)| {
                services
                    .into_iter()
                    .take(service_count)
                    .map(move |service| (state, ingress, service))
            })
    };
    let groups = family.matching(source);
    let candidates = groups.clone().flat_map(|group| {
        selectors().filter_map(move |selector| {
            let index = group
                .interfaces
                .binary_search_by_key(&selector, |group| group.selector)
                .ok()?;
            group.interfaces[index].matching(destination)
        })
    });
    let destination_sets = set_index.matching(destination);
    let set_matches = groups
        .filter(|group| !group.sets.entries.is_empty())
        .flat_map(|group| {
            destination_sets.clone().flat_map(move |set| {
                selectors().filter_map(move |selector| {
                    group
                        .sets
                        .matching(set, selector)
                        .map(|actions| (&group.sets, actions))
                })
            })
        });
    let control = candidates
        .clone()
        .filter_map(|(group, index, parents)| group.next_control(index, parents, start))
        .chain(set_matches.clone().filter_map(|(set_rules, actions)| {
            next_control(&set_rules.controls[actions.controls.clone()], start)
        }))
        .min_by_key(|entry| entry.order);
    if family.has_marks {
        let orders = start..control.map_or(usize::MAX, |entry| entry.order);
        // 计数是观测副作用。保留规则即使没有当前计数，仍须执行截止点之前的赋标。
        let latest = candidates
            .filter(|(group, index, _)| group.actions[*index].has_marks)
            .flat_map(|(group, index, parents)| {
                group.marks_in(core::iter::once(index).chain(parents), orders.clone())
            })
            .chain(set_matches.flat_map(|(set_rules, actions)| {
                marks_in(&set_rules.marks[actions.marks.clone()], orders.clone())
            }))
            .inspect(|entry| hit(entry.order))
            .max();
        if let Some(latest) = latest {
            *mark = latest.value;
        }
    }
    if let Some(control) = control {
        hit(control.order);
    }
    control
}
