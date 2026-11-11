//! 预路由匹配按入接口分组，复用数据面的前缀索引；规则次序独立于前缀长度。

use super::SocketError;
use alloc::{string::String, sync::Arc, vec::Vec};
use core::{net::IpAddr, ops::Range};
use native_runtime::native_abi::{self, Status};
use prefix_index::{Address, Poptrie};
use router_protocol::firewall::{
    ConnectionState, Destination, Ingress, MAX_JUMP_DEPTH, Prerouting, PreroutingAction, counters,
};

type Selector = (Option<ConnectionState>, u32);

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
    fn matching(&self, destination: A) -> Option<(&Self, usize, impl Iterator<Item = usize> + '_)> {
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
            marks_in(&self.marks[self.actions[index].marks.clone()], orders.clone())
        })
    }

    fn next_control(&self, index: usize, parents: impl Iterator<Item = usize>, start: usize) -> Option<Control> {
        let first = self.actions[index].first_control?;
        if first.order >= start {
            return Some(first);
        }
        // 返回后要越过已处理的跳转。最早候选仍只来自本次命中的前缀和祖先。
        core::iter::once(index).chain(parents)
            .filter_map(|index| next_control(&self.controls[self.actions[index].controls.clone()], start))
            .min_by_key(|entry| entry.order)
    }
}

struct SetRule {
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
    fn compile(mut rules: Vec<SetRule>) -> Result<Self, SocketError> {
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

struct SetIndex<A> {
    destinations: Poptrie<A>,
    members: Vec<u32>,
}

impl<A: Address> Default for SetIndex<A> {
    fn default() -> Self {
        Self {
            destinations: Poptrie::default(),
            members: Vec::new(),
        }
    }
}

impl<A: Address> SetIndex<A> {
    fn build(mut prefixes: Vec<(u32, A, u8)>) -> Result<Self, SocketError> {
        // 1. 同一集合的规范 CIDR 按地址、长度排列，前缀覆盖关系可以从最后保留项判断。
        //    去掉重叠成员后，每个目的地址最多为同一集合产生一个命中。
        prefixes.sort_unstable();
        prefixes.dedup_by(|later, earlier| {
            later.0 == earlier.0 && earlier.2 <= later.2 && later.1.masked(earlier.2) == earlier.1
        });
        // 2. 不同集合可包含同一前缀。索引的引用区间对应这些集合下标。
        prefixes.sort_unstable_by_key(|&(set, network, length)| (network, length, set));
        let destinations = Poptrie::build(
            prefixes
                .iter()
                .map(|&(_, network, length)| (network, length)),
        )
        .map_err(prefix_error)?;
        let mut members = Vec::new();
        members
            .try_reserve_exact(prefixes.len())
            .map_err(|_| SocketError::NoMemory)?;
        members.extend(prefixes.into_iter().map(|(set, _, _)| set));
        Ok(Self {
            destinations,
            members,
        })
    }

    fn matching(&self, address: A) -> impl Iterator<Item = u32> + Clone + '_ {
        self.destinations
            .matching(address)
            .flat_map(|prefix| self.members[prefix.range()].iter().copied())
    }

    fn same_members(&self, other: &Self) -> bool {
        let membership =
            |prefix: &prefix_index::Prefix<A>| (prefix.network, prefix.length, prefix.range());
        self.members == other.members
            && self.destinations.prefixes().iter().map(membership).eq(other
                .destinations
                .prefixes()
                .iter()
                .map(membership))
    }
}

#[derive(Default)]
struct AddressSets {
    ipv4: SetIndex<u32>,
    ipv6: SetIndex<u128>,
    tracks: [bool; 2],
}

impl AddressSets {
    fn compile(chain: Prerouting<'_>, rules: &Classification) -> Result<Self, SocketError> {
        let mut ipv4 = Vec::new();
        let mut ipv6 = Vec::new();
        let mut tracks = [false; 2];
        for (index, set) in chain.address_sets().enumerate() {
            let Some(tracked) = rules.set_tracking[index] else {
                continue;
            };
            for prefix in set.members() {
                match prefix.network {
                    IpAddr::V4(address) => {
                        ipv4.try_reserve(1).map_err(|_| SocketError::NoMemory)?;
                        ipv4.push((index as u32, u32::from(address), prefix.length));
                        tracks[0] |= tracked;
                    }
                    IpAddr::V6(address) => {
                        ipv6.try_reserve(1).map_err(|_| SocketError::NoMemory)?;
                        ipv6.push((index as u32, u128::from(address), prefix.length));
                        tracks[1] |= tracked;
                    }
                }
            }
        }
        Ok(Self {
            ipv4: SetIndex::build(ipv4)?,
            ipv6: SetIndex::build(ipv6)?,
            tracks,
        })
    }

    fn same_members(&self, other: &Self) -> bool {
        self.ipv4.same_members(&other.ipv4) && self.ipv6.same_members(&other.ipv6)
    }
}

/// 不可变规则定义和决策索引。集合成员变化时复用同一份规则与名字。
pub(super) struct Classification {
    ipv4: Vec<InterfaceRules<u32>>,
    ipv6: Vec<InterfaceRules<u128>>,
    sets: SetRules,
    rules: Vec<router_protocol::firewall::Rule>,
    set_names: Vec<String>,
    // None 表示集合只供设备缺席的规则引用，不安装成员索引。
    set_tracking: Vec<Option<bool>>,
}

impl Classification {
    fn matches(&self, chain: Prerouting<'_>) -> bool {
        self.rules.iter().copied().eq(chain.rules())
            && self
                .set_names
                .iter()
                .map(String::as_str)
                .eq(chain.address_sets().map(|set| set.name))
    }

    fn compile(chain: Prerouting<'_>) -> Result<Self, SocketError> {
        let mut ipv4 = Vec::new();
        let mut ipv6 = Vec::new();
        let mut set_rules = Vec::new();
        let mut rules = Vec::new();
        let mut set_names = Vec::new();
        let mut set_tracking = Vec::new();
        let set_count = chain.address_sets().len();
        rules
            .try_reserve_exact(chain.rules().len())
            .map_err(|_| SocketError::NoMemory)?;
        set_names
            .try_reserve_exact(set_count)
            .map_err(|_| SocketError::NoMemory)?;
        set_tracking
            .try_reserve_exact(set_count)
            .map_err(|_| SocketError::NoMemory)?;
        set_tracking.resize(set_count, None);
        for set in chain.address_sets() {
            set_names.push(capture_name(set.name).map_err(|_| SocketError::NoMemory)?);
        }
        for (order, rule) in chain.rules().enumerate() {
            rules.push(rule);
            let ingress = match rule.ingress {
                Ingress::Any => 0,
                Ingress::Interface(index) => index,
                Ingress::Unavailable => continue,
            };
            let selector = (rule.connection_state, ingress);
            let action = rule.action;
            match rule.destination {
                Destination::Prefix(prefix) => match prefix.network {
                    IpAddr::V4(network) => {
                        ipv4.try_reserve(1).map_err(|_| SocketError::NoMemory)?;
                        ipv4.push(Rule {
                            selector,
                            network: u32::from(network),
                            length: prefix.length,
                            order,
                            action,
                        });
                    }
                    IpAddr::V6(network) => {
                        ipv6.try_reserve(1).map_err(|_| SocketError::NoMemory)?;
                        ipv6.push(Rule {
                            selector,
                            network: u128::from(network),
                            length: prefix.length,
                            order,
                            action,
                        });
                    }
                },
                Destination::Set(index) => {
                    let tracking = &mut set_tracking[index as usize];
                    *tracking = Some(tracking.unwrap_or(false) || rule.connection_state.is_some());
                    set_rules
                        .try_reserve(1)
                        .map_err(|_| SocketError::NoMemory)?;
                    set_rules.push(SetRule {
                        key: (index, selector),
                        order,
                        action,
                    });
                }
            }
        }
        Ok(Self {
            ipv4: compile(&mut ipv4)?,
            ipv6: compile(&mut ipv6)?,
            sets: SetRules::compile(set_rules)?,
            rules,
            set_names,
            set_tracking,
        })
    }
}

#[derive(Clone, Copy, Default)]
struct Hits {
    packets: u64,
    bytes: u64,
}

#[derive(Default)]
pub(super) struct Firewall {
    classification: Option<Arc<Classification>>,
    address_sets: AddressSets,
    // NET guard 独占生效计数。发布和撤销时，计数随所属规则链交换。
    counters: Vec<Hits>,
    revision: u64,
}

/// 从默认域捕获一页计数；编码与传输在释放网络锁后执行。
/// # Errors
/// 域不可用、规则链换版、游标越界或分配失败。
pub(crate) fn counter_page(
    scope: &crate::namespace::Namespace,
    cursor: counters::Cursor,
) -> Result<counters::Page, Status> {
    super::stack(scope)
        .map_err(|_| native_abi::ERR_BAD_STATE)?
        .lock()
        .map_err(|_| native_abi::ERR_NO_MEMORY)?
        .firewall
        .counter_page(cursor)
}

impl Firewall {
    /// 捕获可由新配置共享的不可变规则索引，不持有网络锁进入编译。
    pub(super) fn classification(&self) -> Option<Arc<Classification>> {
        self.classification.clone()
    }

    /// 编译候选地址集合及需要替换的规则，预备失败保留生效版本。
    /// # Errors
    /// 索引容量、成员或存储分配失败。
    pub(super) fn compile(
        chain: Prerouting<'_>,
        inherited: Option<Arc<Classification>>,
    ) -> Result<Self, SocketError> {
        if chain.rules().len() == 0 {
            return Ok(Self::default());
        }
        let classification =
            if let Some(inherited) = inherited.filter(|current| current.matches(chain)) {
                inherited
            } else {
                Arc::try_new(Classification::compile(chain)?).map_err(|_| SocketError::NoMemory)?
            };
        let address_sets = AddressSets::compile(chain, &classification)?;
        let mut counters = Vec::new();
        counters
            .try_reserve_exact(classification.rules.len())
            .map_err(|_| SocketError::NoMemory)?;
        counters.resize(classification.rules.len(), Hits::default());
        let mut revision = [0; 8];
        native_runtime::system::cprng_draw(&mut revision);
        Ok(Self {
            classification: Some(classification),
            address_sets,
            counters,
            revision: u64::from_le_bytes(revision).max(2),
        })
    }

    fn same_rules(&self, other: &Self) -> bool {
        match (&self.classification, &other.classification) {
            (None, None) => true,
            (Some(left), Some(right)) => {
                Arc::ptr_eq(left, right)
                    || (left.rules == right.rules && left.set_names == right.set_names)
            }
            _ => false,
        }
    }

    /// 判断候选规则及实际集合成员是否与生效投影相同。
    pub(super) fn same_program(&self, other: &Self) -> bool {
        self.same_rules(other) && self.address_sets.same_members(&other.address_sets)
    }

    /// 原子交换配置及前像。只改集合时，已有规则计数随同一规则定义继续累计。
    pub(super) fn exchange(&mut self, other: &mut Self) {
        if self.same_rules(other) {
            core::mem::swap(&mut self.counters, &mut other.counters);
        }
        core::mem::swap(self, other);
    }

    /// 复制同一已发布链的一页计数。查询不会改变计数。
    /// # Errors
    /// 规则链已换版、游标越界或分页缓冲分配失败。
    pub(super) fn counter_page(&self, cursor: counters::Cursor) -> Result<counters::Page, Status> {
        let revision = self.revision.max(1);
        if cursor.revision != 0 && cursor.revision != revision {
            return Err(native_abi::ERR_BAD_STATE);
        }
        let start = cursor.offset as usize;
        let end = start
            .saturating_add(counters::LIMIT)
            .min(self.counters.len());
        let records = self
            .counters
            .get(start..end)
            .ok_or(native_abi::ERR_OUT_OF_RANGE)?;
        let mut rows = Vec::new();
        rows.try_reserve_exact(records.len())
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        if let Some(classification) = &self.classification {
            for (&hits, &rule) in records.iter().zip(&classification.rules[start..end]) {
                let destination_set = match rule.destination {
                    Destination::Prefix(_) => None,
                    Destination::Set(index) => {
                        Some(capture_name(&classification.set_names[index as usize])?)
                    }
                };
                rows.push(counters::Counter {
                    rule,
                    destination_set,
                    packets: hits.packets,
                    bytes: hits.bytes,
                });
            }
        }
        Ok(counters::Page {
            revision,
            next: (end < self.counters.len()).then_some(end as u32),
            rows,
        })
    }

    /// 判断此地址族是否需要提取连接状态。无状态规则时不解析传输头。
    pub(super) fn tracks(&self, ipv6: bool) -> bool {
        self.classification.as_ref().is_some_and(|rules| {
            self.address_sets.tracks[usize::from(ipv6)]
                || if ipv6 {
                    rules
                        .ipv6
                        .last()
                        .is_some_and(|group| group.selector.0.is_some())
                } else {
                    rules
                        .ipv4
                        .last()
                        .is_some_and(|group| group.selector.0.is_some())
                }
        })
    }

    /// 对当前入接口和目的地址求值。返回放行报文的标记，丢弃时返回 None。
    pub(super) fn evaluate(
        &mut self,
        ingress: u32,
        destination: IpAddr,
        state: Option<ConnectionState>,
        bytes: usize,
    ) -> Option<u32> {
        let Some(rules) = &self.classification else {
            return Some(0);
        };
        match destination {
            IpAddr::V4(address) => classify(
                &rules.ipv4,
                &rules.sets,
                &self.address_sets.ipv4,
                &mut self.counters,
                (ingress, u32::from(address), state, bytes),
            ),
            IpAddr::V6(address) => classify(
                &rules.ipv6,
                &rules.sets,
                &self.address_sets.ipv6,
                &mut self.counters,
                (ingress, u128::from(address), state, bytes),
            ),
        }
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
) -> Result<PrefixActions, SocketError> {
    let start = marks.len();
    let mut verdict = None;
    for (order, action) in rules {
        match action {
            PreroutingAction::SetMark { value } => {
                marks.try_reserve(1).map_err(|_| SocketError::NoMemory)?;
                marks.push(Mark { order, value });
            }
            PreroutingAction::Accept | PreroutingAction::Drop => {
                verdict = Some(Verdict {
                    order,
                    drop: action == PreroutingAction::Drop,
                });
                break;
            }
        }
    }
    Ok(PrefixActions {
        marks: start..marks.len(),
        has_marks: start < marks.len(),
        verdict,
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
        // 1. 同一前缀按声明次序保存赋标记录。首个裁决之后同前缀的动作不可到达。
        for prefix in prefixes {
            actions.push(compile_actions(
                group[prefix.range()]
                    .iter()
                    .map(|rule| (rule.order, rule.action)),
                &mut marks,
            )?);
        }
        // 2. 每个前缀缓存覆盖它的首个裁决及此前是否有赋标。无赋标时无需遍历祖先。
        for (index, prefix) in prefixes.iter().enumerate() {
            let covering = destinations
                .matching_indices(prefix.network)
                .filter(|index| prefixes[*index].length <= prefix.length);
            let verdict = covering
                .clone()
                .filter_map(|index| actions[index].verdict)
                .min();
            let stop = verdict.map_or(usize::MAX, |verdict| verdict.order);
            let has_marks = covering
                .filter_map(|index| marks[actions[index].marks.clone()].first())
                .any(|mark| mark.order < stop);
            actions[index].verdict = verdict;
            actions[index].has_marks = has_marks;
        }
        result.try_reserve(1).map_err(|_| SocketError::NoMemory)?;
        result.push(InterfaceRules {
            selector: group[0].selector,
            destinations,
            actions,
            marks,
        });
    }
    Ok(result)
}

fn classify<A: Address>(
    groups: &[InterfaceRules<A>],
    set_rules: &SetRules,
    set_index: &SetIndex<A>,
    counters: &mut [Hits],
    (ingress, destination, state, bytes): (u32, A, Option<ConnectionState>, usize),
) -> Option<u32> {
    if groups.is_empty() && set_rules.entries.is_empty() {
        return Some(0);
    }
    let lookup = |selector| {
        let index = groups
            .binary_search_by_key(&selector, |group| group.selector)
            .ok()?;
        groups[index].matching(destination)
    };
    let selectors = [
        Some((None, 0)),
        Some((None, ingress)),
        state.map(|state| (Some(state), 0)),
        state.map(|state| (Some(state), ingress)),
    ];
    let candidates = selectors.map(|selector| selector.and_then(lookup));
    let set_matches = set_index.matching(destination).flat_map(|set| {
        selectors
            .into_iter()
            .flatten()
            .filter_map(move |selector| set_rules.matching(set, selector))
    });
    let verdict = candidates
        .iter()
        .flatten()
        .filter_map(|(group, index, _)| group.actions[*index].verdict)
        .chain(set_matches.clone().filter_map(|actions| actions.verdict))
        .min();
    let stop = verdict.map_or(usize::MAX, |verdict| verdict.order);
    let mut hit = |order| {
        let counter: &mut Hits = &mut counters[order];
        counter.packets = counter.packets.saturating_add(1);
        counter.bytes = counter.bytes.saturating_add(bytes as u64);
    };
    let mark = candidates
        .into_iter()
        .flatten()
        .filter(|(group, index, _)| group.actions[*index].has_marks)
        .flat_map(|(group, index, parents)| {
            group.marks_before(core::iter::once(index).chain(parents), stop)
        })
        .chain(set_matches.flat_map(|actions| {
            set_rules.marks[actions.marks.clone()]
                .iter()
                .copied()
                .take_while(move |mark| mark.order < stop)
        }))
        .inspect(|mark| hit(mark.order))
        .max();
    if let Some(verdict) = verdict {
        hit(verdict.order);
        if verdict.drop {
            return None;
        }
    }
    Some(mark.map_or(0, |mark| mark.value))
}
