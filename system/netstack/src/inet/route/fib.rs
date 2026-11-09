//! 唯一活动 FIB 的连续记录、共享下一跳与 Poptrie；前缀直接引用自己的 alias 区间。
use super::*;
use network_protocol::RouteChange;
use nexthop::{NextHop, NextHops};

/// 常驻路由只保存前缀/策略属性与下一跳槽；网关和出口不随每个前缀重复。
#[derive(Clone, Copy)]
struct StoredRecord {
    destination: Ipv4Addr,
    prefix_length: u8,
    table: u32,
    metric: u32,
    scope: u8,
    protocol: u8,
    kind: Ipv4RouteKind,
    preferred_source: Option<Ipv4Addr>,
    metrics: Ipv4RouteMetrics,
    // 无下一跳形态保留请求标志；有下一跳时只读取对象的声明，具名更新不改前缀。
    on_link: bool,
    origin: Origin,
    next_hop: Option<nexthop::Id>,
}
impl StoredRecord {
    fn new(record: Record, next_hop: Option<nexthop::Id>) -> Self {
        let request = record.request;
        Self {
            destination: request.destination,
            prefix_length: request.prefix_length,
            table: request.table,
            metric: request.metric,
            scope: request.scope,
            protocol: request.protocol,
            kind: request.kind,
            preferred_source: request.preferred_source,
            metrics: request.metrics,
            on_link: next_hop.is_none() && request.on_link,
            origin: record.origin,
            next_hop,
        }
    }
    fn resolve(self, hops: &NextHops) -> Record {
        let resolved = self.next_hop.map(|index| hops.get(index));
        self.project(resolved)
    }
    fn project(self, resolved: Option<(NextHop, Option<nexthop::Name>)>) -> Record {
        let hop = resolved.map(|(hop, _)| hop);
        let interface_index = hop.map(|hop| hop.interface_index);
        Record {
            request: Ipv4RouteRequest {
                destination: self.destination,
                prefix_length: self.prefix_length,
                table: self.table,
                metric: self.metric,
                scope: self.scope,
                protocol: self.protocol,
                kind: self.kind,
                interface_index,
                gateway: hop.and_then(|hop| hop.gateway),
                on_link: hop.map_or(self.on_link, |hop| hop.on_link),
                preferred_source: self.preferred_source,
                metrics: self.metrics,
            },
            origin: self.origin,
            next_hop_name: resolved.and_then(|(_, name)| name),
            interface_index,
        }
    }
}
impl StoredRecord {
    fn select(
        &self,
        hops: &NextHops,
        flow: Flow,
        interfaces: impl Clone + Iterator<Item = InterfaceFacts>,
    ) -> Option<Record> {
        let stored = *self;
        let Some(index) = stored.next_hop else {
            // 无出口记录表示表/终止动作；不能按 oif 提前过滤，否则会错误落到父前缀/默认网关。
            // route_output 必须先接收该动作，再决定明确出口的直接发送 fallback。
            return Some(stored.project(None));
        };
        let (members, name) = hops.view(index);
        let seed = name.map_or_else(|| u64::from(index.get()), |name| name.get());
        let mut member = group::select(members, flow, seed, stored.kind, interfaces.clone())?;
        let mut frozen_source = None;
        // 只在同一匹配组内按最终源重选；常态直接投影选中成员，不先复制首成员记录。
        if members.len() > 1 && flow.source.is_unspecified() {
            frozen_source = stored.preferred_source.or_else(|| {
                interfaces
                    .clone()
                    .find(|interface| interface.index == member.interface_index)
                    .and_then(|interface| interface.address)
            });
            if let Some(source) = frozen_source {
                member = group::select(
                    members,
                    Flow { source, ..flow },
                    seed,
                    stored.kind,
                    interfaces,
                )?;
            }
        }
        let mut record = stored.project(Some((member, name)));
        if frozen_source.is_some() {
            record.request.preferred_source = frozen_source;
        }
        Some(record)
    }
}

#[derive(Clone, Copy)]
pub(super) struct Entry {
    record: StoredRecord,
    order: i128,
}
impl Entry {
    fn prefix(&self) -> (u32, u8) {
        (
            u32::from(self.record.destination),
            self.record.prefix_length,
        )
    }
    fn alias(&self) -> (u32, u32, i128) {
        (self.record.table, self.record.metric, self.order)
    }
    fn key(&self) -> ((u32, u8), (u32, u32, i128)) {
        (self.prefix(), self.alias())
    }
    pub(super) fn table(&self) -> u32 {
        self.record.table
    }
}

fn next_hop(record: Record) -> Option<NextHop> {
    record.interface_index.map(|interface_index| NextHop {
        interface_index,
        gateway: record.request.gateway,
        on_link: record.request.on_link,
        weight: 1,
    })
}

/// 冷准备中的唯一候选；索引尚未编译，不可交给报文读者。
/// 直接构造最终记录数组，不为百万路由再暂存一份完整的逐条解析结果。
pub(super) struct Draft {
    entries: Vec<Entry>,
    hops: NextHops,
}
impl Draft {
    pub(super) fn new(capacity: usize) -> Result<Self, SocketError> {
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(capacity)
            .map_err(|_| SocketError::NoMemory)?;
        Ok(Self {
            entries,
            hops: NextHops::new(),
        })
    }
    pub(super) fn inherit_named(&mut self, fib: &Fib) -> Result<(), SocketError> {
        for definition in fib.hops.definitions() {
            self.define(definition)?;
        }
        Ok(())
    }
    pub(super) fn define(
        &mut self,
        definition: network_protocol::routes::NextHopConfiguration<'_>,
    ) -> Result<(), SocketError> {
        self.define_group(definition.id, group::Group::capture(definition)?)
    }
    pub(super) fn define_group(&mut self, id: u64, group: group::Group) -> Result<(), SocketError> {
        self.hops
            .define(nexthop::Name::new(id).ok_or(SocketError::Invalid)?, group)
    }
    pub(super) fn named(&self, name: nexthop::Name) -> Result<&group::Group, SocketError> {
        self.hops.named_group(name)
    }
    pub(super) fn retire_interface(&mut self, index: u32, force: bool) {
        self.hops.retire(index, force);
    }
    pub(super) fn iter(&self) -> impl ExactSizeIterator<Item = Record> + '_ {
        self.entries
            .iter()
            .map(|entry| entry.record.resolve(&self.hops))
    }
    pub(super) fn push(&mut self, record: Record, order: i128) -> Result<(), SocketError> {
        self.entries
            .try_reserve(1)
            .map_err(|_| SocketError::NoMemory)?;
        let prepared = next_hop(record)
            .map(|hop| self.hops.prepare(hop, record.next_hop_name))
            .transpose()?;
        let next_hop = prepared.map(|prepared| self.hops.acquire(prepared, record.request.scope));
        self.entries.push(Entry {
            record: StoredRecord::new(record, next_hop),
            order,
        });
        Ok(())
    }
    pub(super) fn extend(&mut self, other: Self) -> Result<(), SocketError> {
        self.entries
            .try_reserve(other.entries.len())
            .map_err(|_| SocketError::NoMemory)?;
        for entry in other.entries.iter() {
            self.push(entry.record.resolve(&other.hops), entry.order)?;
        }
        Ok(())
    }
    /// 仅用于小规模接口邻接候选；沿同一成员/设备裁决查询，不编译第二张全量 FIB。
    pub(super) fn lookup_record(
        &self,
        table: u32,
        flow: Flow,
        minimum_scope: u8,
        interfaces: impl Clone + Iterator<Item = InterfaceFacts>,
    ) -> Option<Record> {
        self.entries
            .iter()
            .filter(|entry| {
                entry.table() == table
                    && u32::from(flow.destination) & mask(entry.record.prefix_length)
                        == u32::from(entry.record.destination)
            })
            .filter_map(|entry| {
                entry
                    .record
                    .select(&self.hops, flow, interfaces.clone())
                    .filter(|record| record.request.scope >= minimum_scope)
                    .map(|record| (entry, record))
            })
            .min_by_key(|(entry, _)| {
                (
                    core::cmp::Reverse(entry.record.prefix_length),
                    entry.record.metric,
                    entry.order,
                )
            })
            .map(|(_, record)| record)
    }
    pub(super) fn unique_prefixes(&mut self) -> bool {
        self.entries
            .sort_unstable_by_key(|entry| (entry.table(), entry.prefix()));
        !self
            .entries
            .windows(2)
            .any(|pair| (pair[0].table(), pair[0].prefix()) == (pair[1].table(), pair[1].prefix()))
    }
}

pub(super) struct Fib {
    entries: Vec<Entry>,
    hops: NextHops,
    index: poptrie::Poptrie<u32>,
}
impl Fib {
    pub(super) fn new() -> Self {
        Self {
            entries: Vec::new(),
            hops: NextHops::new(),
            index: poptrie::Poptrie::new(),
        }
    }
    pub(super) fn prepare_next_hop(
        &self,
        name: nexthop::Name,
        hop: group::Group,
    ) -> Result<nexthop::Change, SocketError> {
        self.hops.prepare_change(name, hop)
    }
    pub(super) fn exchange_next_hop(&mut self, change: &mut nexthop::Change) -> &group::Group {
        self.hops.exchange(change)
    }
    pub(super) fn iter(&self) -> impl ExactSizeIterator<Item = Record> + '_ {
        self.entries.iter().map(|entry| self.resolve(entry))
    }
    pub(super) fn resolve(&self, entry: &Entry) -> Record {
        entry.record.resolve(&self.hops)
    }
    /// 在同一 NET guard 捕获字典与行；槽号只转换一次，常驻成员不按前缀复制。
    pub(super) fn snapshot_bytes(
        &self,
        interfaces: &[NetworkInterface],
    ) -> Result<Vec<u8>, SocketError> {
        use network_protocol::routes::{NextHopConfiguration, RouteAttributes};
        let mut indices = Vec::new();
        indices
            .try_reserve_exact(self.hops.slot_count())
            .map_err(|_| SocketError::NoMemory)?;
        indices.resize(self.hops.slot_count(), u32::MAX);
        for (index, (id, _)) in self.hops.active_groups().enumerate() {
            indices[id.get() as usize - 1] = index as u32;
        }
        network_protocol::RoutingSnapshot::encode(
            interfaces
                .iter()
                .map(|interface| (interface.index, interface.name)),
            self.hops
                .active_groups()
                .enumerate()
                .map(|(index, (_, members))| NextHopConfiguration::new(index as u64 + 1, members)),
            self.entries.iter().map(|entry| {
                let record = entry.record;
                let attributes = RouteAttributes {
                    destination: record.destination,
                    prefix_length: record.prefix_length,
                    table: record.table,
                    metric: record.metric,
                    scope: record.scope,
                    protocol: record.protocol,
                    kind: record.kind,
                    preferred_source: record.preferred_source,
                    metrics: record.metrics,
                };
                let group = record.next_hop.map(|id| {
                    let index = indices[id.get() as usize - 1];
                    assert_ne!(
                        index,
                        u32::MAX,
                        "FIB route references an unowned snapshot group"
                    );
                    index
                });
                (attributes, group, group.is_none() && record.on_link)
            }),
        )
        .map_err(|error| {
            if error == native_runtime::native_abi::ERR_NO_MEMORY {
                SocketError::NoMemory
            } else {
                SocketError::Invalid
            }
        })
    }
    pub(super) fn matching(&self, address: Ipv4Addr) -> impl Clone + Iterator<Item = &[Entry]> {
        self.index
            .matching(u32::from(address))
            .map(|prefix| &self.entries[prefix.range()])
    }
    /// 暂存新根；未修改的标量属性复制到候选，下一跳只按实际不同的值暂存一次。
    /// 不复制 socket/设备/能力或动态状态，租约排斥其他配置写者；发布后原根只是事务前像。
    pub(super) fn prepare(
        &self,
        mut candidate: Draft,
        replace: impl Fn(&Record) -> bool,
    ) -> Result<Self, SocketError> {
        let kept = self
            .iter()
            .filter(|record| {
                !replace(record)
                    && record.next_hop_name.is_none_or(|name| {
                        candidate
                            .named(name)
                            .is_ok_and(|group| !group.members().is_empty())
                    })
            })
            .count();
        candidate
            .entries
            .try_reserve_exact(kept)
            .map_err(|_| SocketError::NoMemory)?;
        for entry in &self.entries {
            let record = self.resolve(entry);
            if !replace(&record)
                && record.next_hop_name.is_none_or(|name| {
                    candidate
                        .named(name)
                        .is_ok_and(|group| !group.members().is_empty())
                })
            {
                candidate.push(record, entry.order)?;
            }
        }
        candidate.entries.sort_unstable_by_key(Entry::key);
        let index = poptrie::Poptrie::build(candidate.entries.iter().map(Entry::prefix)).map_err(
            |error| match error {
                poptrie::Error::Invalid => SocketError::Invalid,
                poptrie::Error::NoMemory => SocketError::NoMemory,
                poptrie::Error::NoSpace => SocketError::NoSpace,
            },
        )?;
        Ok(Self {
            entries: candidate.entries,
            hops: candidate.hops,
            index,
        })
    }
    pub(super) fn retain(&mut self, mut keep: impl FnMut(&Record) -> bool) {
        self.entries.retain(|entry| {
            if entry
                .record
                .next_hop
                .is_none_or(|index| !self.hops.members(index).is_empty())
                && keep(&entry.record.resolve(&self.hops))
            {
                return true;
            }
            if let Some(index) = entry.record.next_hop {
                self.hops.release(index, entry.record.scope);
            }
            false
        });
        self.index.recount(self.entries.iter().map(Entry::prefix));
        if self.entries.is_empty() {
            self.entries = Vec::new();
            if self.hops.is_empty() {
                *self = Self::new();
            }
        }
    }
    /// 只用于尚未发布的候选：先准备压缩容量，再按原顺序标记移除并重计索引。
    pub(super) fn retire_source_marks(
        &mut self,
        marks: &[u64],
        retired: usize,
    ) -> Result<(), SocketError> {
        let kept = self.entries.len() - retired;
        let mut compact = Vec::new();
        let shrink = kept < self.entries.capacity() / 2;
        if shrink {
            compact
                .try_reserve_exact(kept)
                .map_err(|_| SocketError::NoMemory)?;
        }
        let mut position = 0;
        self.retain(|_| {
            let keep = marks[position / 64] & (1_u64 << (position % 64)) == 0;
            position += 1;
            keep
        });
        if shrink {
            compact.append(&mut self.entries);
            self.entries = compact;
        }
        Ok(())
    }
    pub(super) fn remove_missing(&mut self, interfaces: &[NetworkInterface]) {
        self.hops.remove_missing(interfaces);
        self.retain(|record| {
            record
                .interface_index
                .is_none_or(|index| interfaces.iter().any(|interface| interface.index == index))
        });
    }
    pub(super) fn retire_interface(&mut self, index: u32, force: bool) {
        self.hops.retire(index, force);
        self.retain(|record| {
            record.interface_index != Some(index) || (!force && record.request.scope == 254)
        });
    }
    pub(super) fn is_multipath(&self, name: Option<nexthop::Name>) -> bool {
        name.is_some_and(|name| {
            self.hops
                .named_group(name)
                .is_ok_and(|group| group.members().len() > 1)
        })
    }
    pub(super) fn select_record(
        &self,
        entry: &Entry,
        flow: Flow,
        interfaces: impl Clone + Iterator<Item = InterfaceFacts>,
    ) -> Option<Record> {
        entry.record.select(&self.hops, flow, interfaces)
    }

    fn group(&self, network: u32, length: u8) -> core::ops::Range<usize> {
        self.index.range_for(network, length).unwrap_or_else(|| {
            let position = self
                .entries
                .partition_point(|entry| entry.prefix() < (network, length));
            position..position
        })
    }
    pub(super) fn insert(
        &mut self,
        record: Record,
        change: RouteChange,
    ) -> Result<(), Ipv4RouteError> {
        // 设备准入已解析 actual oif；与快照一样按实际下一跳比较，不保存未绑定输入的副本。
        let request = Ipv4RouteRequest {
            interface_index: record.interface_index,
            ..record.request
        };
        let network = u32::from(request.destination);
        let group = self.group(network, request.prefix_length);
        let from = group.start
            + self.entries[group.clone()].partition_point(|entry| {
                (entry.record.table, entry.record.metric) < (request.table, request.metric)
            });
        let mut first = None;
        let mut exact = None;
        for (offset, entry) in self.entries[from..group.end].iter().enumerate() {
            let index = from + offset;
            if (entry.record.table, entry.record.metric) != (request.table, request.metric) {
                break;
            }
            first.get_or_insert(index);
            if self.resolve(entry).request == request
                && self.resolve(entry).next_hop_name == record.next_hop_name
            {
                exact = Some(index);
                break;
            }
        }
        if change.exclusive && first.is_some() || !change.replace && exact.is_some() {
            return Err(Ipv4RouteError::Exists);
        }
        if change.replace && exact.is_some() {
            return if exact == first {
                Ok(())
            } else {
                Err(Ipv4RouteError::Exists)
            };
        }
        if !change.create && first.is_none() {
            return Err(Ipv4RouteError::Missing);
        }
        let replacement = first.filter(|_| change.replace);
        if let Some(index) = replacement {
            let previous = self.entries[index].record.next_hop;
            if previous.map(|index| self.hops.get(index))
                == next_hop(record).map(|hop| (hop, record.next_hop_name))
            {
                if let Some(hop) = previous {
                    self.hops.change_scope(
                        hop,
                        self.entries[index].record.scope,
                        record.request.scope,
                    );
                }
                // 同一下一跳只替换前缀属性；引用数不变，不为瞬态 +1 制造容量失败。
                self.entries[index].record = StoredRecord::new(record, previous);
                return Ok(());
            }
        }
        let addition = if replacement.is_none() {
            let addition = self
                .index
                .prepare_add(network, request.prefix_length)
                .map_err(|_| Ipv4RouteError::NoMemory)?;
            self.entries
                .try_reserve(1)
                .map_err(|_| Ipv4RouteError::NoMemory)?;
            Some(addition)
        } else {
            None
        };
        let prepared = next_hop(record)
            .map(|hop| self.hops.prepare(hop, record.next_hop_name))
            .transpose()
            .map_err(|_| Ipv4RouteError::NoMemory)?;
        let next_hop = prepared.map(|prepared| self.hops.acquire(prepared, record.request.scope));
        let stored = StoredRecord::new(record, next_hop);
        if let Some(index) = replacement {
            if let Some(retired) = self.entries[index].record.next_hop {
                self.hops.release(retired, self.entries[index].record.scope);
            }
            self.entries[index].record = stored;
        } else {
            let order = i128::from(crate::identity::next());
            let entry = Entry {
                record: stored,
                order: if change.append { order } else { -order },
            };
            let position = group.start
                + self.entries[group].partition_point(|current| current.alias() < entry.alias());
            self.entries.insert(position, entry);
            self.index
                .add(addition.expect("new alias prepared its prefix"));
        }
        Ok(())
    }
    pub(super) fn remove(&mut self, request: Ipv4RouteRequest) -> Result<(), Ipv4RouteError> {
        let network = u32::from(request.destination);
        let found = self
            .group(network, request.prefix_length)
            .into_iter()
            .find(|index| {
                let record = self.resolve(&self.entries[*index]);
                record.request.table == request.table
                    && (request.metric == 0 || request.metric == record.request.metric)
                    && (request.protocol == 0 || request.protocol == record.request.protocol)
                    && request
                        .interface_index
                        .is_none_or(|index| record.interface_index == Some(index))
                    && request
                        .gateway
                        .is_none_or(|gateway| record.request.gateway == Some(gateway))
                    && request.kind == record.request.kind
            })
            .ok_or(Ipv4RouteError::NotFound)?;
        let retired = self.entries.remove(found).record;
        if let Some(index) = retired.next_hop {
            self.hops.release(index, retired.scope);
        }
        self.index.remove(network, request.prefix_length);
        if self.entries.is_empty() {
            self.entries = Vec::new();
            if self.hops.is_empty() {
                *self = Self::new();
            }
        }
        Ok(())
    }
}
