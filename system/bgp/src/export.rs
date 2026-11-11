//! 每条 TCP 的实际发送记录与最早可发工作；不镜像 Loc-RIB，也不在队列中冻结过期的完整路由表。
mod wire;
use crate::{
    AttributeChanges, NextHop, Peer, PeerKind, RouteAttributes, RoutingDomain, import::action,
};
use alloc::{rc::Rc, vec::Vec};
use core::net::{IpAddr, Ipv4Addr};
use fallible_tree::{FallibleMap, NodeSlot};
use packet_wire::{
    PacketError,
    bgp::{
        attributes::AsnWidth,
        nlri::{Family, Prefix},
    },
};
use router_protocol::{
    bgp::{ExportPolicy, MedAction, RouteAction},
    policy::Disposition,
};

/// 同一次发送的冻结会话投影；地址来自实际 TCP，本地 Identifier 不能代替下一跳。
#[derive(Clone, Copy)]
pub struct ExportContext {
    /// 本连接的真实本地 AS：联盟外部为公开身份，成员间为成员 AS，不使用两字节 My AS 投影。
    pub local_as: u32,
    /// 已连接 TCP 的实际本地地址，不是 BGP Identifier。
    pub local_address: IpAddr,
    /// 本次 TCP 已协商的 IPv6 单播能力；未协商不能发送 MP IPv6 UPDATE。
    pub ipv6_unicast: bool,
    /// 本次 TCP 已协商的基础 IPv4 单播能力，不能以“默认支持”替代协商结果。
    pub ipv4_unicast: bool,
    /// 对端明确接受 IPv4 单播的 IPv6 下一跳，且已协商该 MP 地址族。
    pub send_ipv4_via_ipv6: bool,
    /// 该域的不可变有效反射簇，普通 iBGP 为 None。
    pub cluster_id: Option<Ipv4Addr>,
    /// 当前 TCP 方向的不可变已协商邻居身份。
    pub target: Peer,
    /// 对端实际协商的 AS 号宽度。
    pub width: AsnWidth,
    /// 每前缀宣告或撤回之间的最短间隔。
    pub interval_ns: u64,
}
/// 出口准备失败；不得发送失败编码或把部分发送记作完成。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportError {
    NoMemory,
    Invalid,
    Packet(PacketError),
}
impl From<PacketError> for ExportError {
    fn from(error: PacketError) -> Self {
        Self::Packet(error)
    }
}
struct Advertisement {
    attributes: RouteAttributes,
    next_hop: NextHop,
    preference: Option<u32>,
    // 反射时只持起源标量；簇身份随连接冻结，原始列表仍借用共享属性，不按路由复制。
    reflected_originator: Option<Ipv4Addr>,
}
impl Advertisement {
    fn same_output(&self, other: &Self, kind: PeerKind) -> bool {
        let (a, b) = (&self.attributes, &other.attributes);
        let external = kind == PeerKind::External;
        let ordinary = |kind| {
            matches!(
                kind,
                packet_wire::bgp::attributes::SegmentKind::Sequence
                    | packet_wire::bgp::attributes::SegmentKind::Set
            )
        };
        // 同一驻留属性的比较是共享身份快路；过滤后的不同原值仍须比较实际出口字段。
        self.reflected_originator == other.reflected_originator
            && self.next_hop == other.next_hop
            && self.preference == other.preference
            && (a == b
                || a.origin() == b.origin()
                    && a.med() == b.med()
                    && a.aggregator() == b.aggregator()
                    && a.atomic_aggregate() == b.atomic_aggregate()
                    && a.path()
                        .segments()
                        .filter(|(kind, _)| !external || ordinary(*kind))
                        .eq(b
                            .path()
                            .segments()
                            .filter(|(kind, _)| !external || ordinary(*kind)))
                    && a.communities() == b.communities()
                    && a.large_communities() == b.large_communities()
                    && a.extended_communities()
                        .iter()
                        .filter(|value| !external || value[0] & 0x40 == 0)
                        .eq(b
                            .extended_communities()
                            .iter()
                            .filter(|value| !external || value[0] & 0x40 == 0))
                    && [8, 16, 17, 32]
                        .into_iter()
                        .all(|code| a.is_partial(code) == b.is_partial(code))
                    && a.unknown_transitive().eq(b.unknown_transitive())
                    && (kind != PeerKind::Internal
                        || (self.reflected_originator.is_some()
                            || a.originator_id() == b.originator_id())
                            && a.cluster_list() == b.cluster_list()))
    }
}
fn same_output(a: Option<&Advertisement>, b: Option<&Advertisement>, kind: PeerKind) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => a.same_output(b, kind),
        (None, None) => true,
        _ => false,
    }
}

struct Record {
    // OWNER: 同一单线程 actor 的不可变发送投影；同批前缀共享，避免每条记录复制全局/链路下一跳和属性句柄。
    // Rc 不跨线程，没有原子或新驻留池；最后一个发送记录/在途帧退休时释放。撤回不创建投影。
    route: Option<Rc<Advertisement>>,
    sent_at: Option<u64>,
    pending_at: Option<u64>,
    // OWNER: 此记录在有界重算中的义务；远端重发不能被去重，网络重算只发布实际变化，编码/退休消费。
    refresh_pending: Option<RefreshMode>,
}
// 一帧内每个目标只保留身份和撤回后的预留清理节点；相同已编码属性由整批唯一持有。
struct FlightPrefix {
    prefix: Prefix,
    expiry: Option<NodeSlot<(u64, Prefix), ()>>,
}
enum Flight {
    Routes {
        prefixes: Vec<FlightPrefix>,
        route: Option<Rc<Advertisement>>,
        interval: u64,
    },
    End(Family),
    RefreshRequest(Family),
    RefreshBoundary {
        family: Family,
        begin: bool,
    },
}
// OWNER: 一次刷新只持有索引游标；后续请求合并为一轮跟进，不能中途反复倒退游标饿死后面的目标。
struct Refresh {
    progress: RefreshProgress,
    mode: RefreshMode,
    again: Option<RefreshMode>,
}
#[derive(Clone, Copy)]
enum RefreshProgress {
    Starting,
    Scanning(Option<Prefix>),
    Draining,
}
impl Refresh {
    fn new(mode: RefreshMode) -> Self {
        Self {
            progress: if mode == RefreshMode::Enhanced {
                RefreshProgress::Starting
            } else {
                RefreshProgress::Scanning(None)
            },
            mode,
            again: None,
        }
    }
}
// 重发是远端请求的报文义务；重算只在实际出口变化时发包。二者共享游标与有界积压，不保存第二份表。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum RefreshMode {
    Reconsider,
    Reannounce,
    Enhanced,
}
// 每次最多补齐这些待发目标，发送背压/MRAI 阻塞时不继续展开全表；队列没有百万条刷新副本。
const REFRESH_BATCH: usize = 32;
// 同一 actor 每轮有界处理，避免全表出口阻塞 HoldTimer/其他邻居；NLRI 临时缓冲同此上限。
const EXPORT_BATCH: usize = 128;

/// 单 TCP 的 Adj-RIB-Out；发送记录只在完整送入该 TCP 后更新，断线必须整体丢弃。
pub struct AdjRibOut {
    // OWNER: 记录实际已发送值与同前缀唯一工作索引；ready 是该 owner 的截止索引，不持第二份路由。
    records: FallibleMap<Prefix, Record>,
    ready: FallibleMap<(u64, Prefix), ()>,
    // OWNER: 至多一份已编码在途 UPDATE；后续变更可重新排队，不能取消已经部分写出的 TCP 帧。
    flight: Option<Flight>,
    // OWNER: 初始同步结束标记，只在全部初始工作发送后生成；部分发送不提前清除此义务。
    end_pending: [bool; 2],
    // OWNER: 此 owner 在 sent 时维护的已发送聚合计数，避免状态查询扫描百万条记录；在途不计入。
    advertised: usize,
    // OWNER: 最近一次因报文上限或成员 AS 不可表示而不能发布的目标及原因；只作诊断，不参与选路或保存属性副本。
    rejected: Option<(Prefix, PacketError)>,
    refresh: [Option<Refresh>; 2],
    // OWNER: 每族最多一个管理端请求，实际写完前保持排队。连接退出时随该出口整体丢弃。
    refresh_requests: [bool; 2],
    // OWNER: refresh_pending 的同 owner 聚合，仅用于限制刷新额外积压；普通更新繁忙不能因此饿死刷新。
    refresh_queued: [usize; 2],
}
impl Default for AdjRibOut {
    fn default() -> Self {
        Self::new()
    }
}
impl AdjRibOut {
    /// 创建空连接记录；尚未启动初始同步，不分配。
    pub const fn new() -> Self {
        Self {
            records: FallibleMap::new(),
            ready: FallibleMap::new(),
            flight: None,
            end_pending: [false; 2],
            advertised: 0,
            rejected: None,
            refresh: [None, None],
            refresh_requests: [false; 2],
            refresh_queued: [0; 2],
        }
    }
    /// 已完成指定族的初始登记；所有初始工作送出后发送该族 EoR。
    pub fn finish_initial(&mut self, family: Family) {
        self.end_pending[usize::from(family == Family::Ipv6)] = true;
    }
    /// 常量时间登记该族的重发请求，保留一轮跟进；不复制全表或取消部分发送。
    /// `enhanced` 表示已协商 RFC 7313，首尾需要在同一 TCP 上发送边界。
    pub fn refresh(&mut self, family: Family, enhanced: bool) {
        self.rescan(
            family,
            if enhanced {
                RefreshMode::Enhanced
            } else {
                RefreshMode::Reannounce
            },
        );
    }
    /// 登记一个已由 Session 检查方向能力的本地主动请求。
    /// 同族在途或已排队请求合并，与响应对端请求的出口扫描相互独立。
    pub fn request_refresh(&mut self, family: Family) {
        self.refresh_requests[usize::from(family == Family::Ipv6)] = true;
    }
    /// 网络资格变化后有界重算该族，包括先前未宣告的目标；实际出口未变时不发送重复报文。
    pub fn reconsider(&mut self, family: Family) {
        self.rescan(family, RefreshMode::Reconsider);
    }
    fn rescan(&mut self, family: Family, mode: RefreshMode) {
        let owner = &mut self.refresh[usize::from(family == Family::Ipv6)];
        if let Some(refresh) = owner {
            refresh.again = Some(refresh.again.map_or(mode, |old| old.max(mode)));
        } else {
            *owner = Some(Refresh::new(mode));
        }
    }
    fn refresh_scanning(&self, slot: usize) -> bool {
        self.refresh[slot]
            .as_ref()
            .is_some_and(|refresh| matches!(refresh.progress, RefreshProgress::Scanning(_)))
    }
    fn refresh_boundary(&self) -> Option<(Family, bool)> {
        self.refresh.iter().enumerate().find_map(|(slot, refresh)| {
            let begin = match refresh.as_ref()?.progress {
                RefreshProgress::Starting => true,
                RefreshProgress::Draining if self.refresh_queued[slot] == 0 => false,
                _ => return None,
            };
            Some((
                if slot == 0 {
                    Family::Ipv4
                } else {
                    Family::Ipv6
                },
                begin,
            ))
        })
    }
    fn queue_refresh(&mut self, domain: &RoutingDomain, now: u64) -> Result<(), ExportError> {
        // 1. 普通工作再多也推进游标，但发送背压/MRAI 堵塞时不展开整表。
        for round in 0..REFRESH_BATCH {
            let slot = match (self.refresh_scanning(0), self.refresh_scanning(1)) {
                (false, false) => break,
                (true, false) => 0,
                (false, true) => 1,
                (true, true) => round % 2,
            };
            if self.refresh_queued.iter().sum::<usize>() >= REFRESH_BATCH {
                break;
            }
            let refresh = self.refresh[slot].as_ref().expect("active rescan cursor");
            let RefreshProgress::Scanning(after) = refresh.progress else {
                unreachable!("only an active cursor is scheduled")
            };
            let family = if slot == 0 {
                Family::Ipv4
            } else {
                Family::Ipv6
            };
            let recorded = match after {
                None => self.records.ceiling(&Prefix {
                    address: if slot == 0 {
                        core::net::Ipv4Addr::UNSPECIFIED.into()
                    } else {
                        core::net::Ipv6Addr::UNSPECIFIED.into()
                    },
                    length: 0,
                }),
                Some(prefix) => self.records.successor(&prefix),
            }
            .map(|(prefix, record)| (*prefix, record.route.is_some()))
            .filter(|(prefix, _)| usize::from(prefix.address.is_ipv6()) == slot);
            // 2. 合并实际发送记录与当前三来源视图：旧目标能撤回，之前被拒绝的目标也能恢复。
            let intended = (refresh.mode == RefreshMode::Reconsider)
                .then(|| domain.next_announcement(family, after))
                .flatten();
            let next = recorded
                .map(|(prefix, _)| prefix)
                .into_iter()
                .chain(intended)
                .min();
            let Some(prefix) = next else {
                if refresh.mode == RefreshMode::Enhanced {
                    self.refresh[slot].as_mut().expect("active cursor").progress =
                        RefreshProgress::Draining;
                } else {
                    self.refresh[slot] = refresh.again.map(Refresh::new);
                }
                continue;
            };
            let mode = refresh.mode;
            if mode >= RefreshMode::Reannounce
                && recorded.is_some_and(|(_, advertised)| !advertised)
            {
                self.refresh[slot]
                    .as_mut()
                    .expect("active rescan cursor")
                    .progress = RefreshProgress::Scanning(Some(prefix));
                continue;
            }
            self.changed(prefix, now)?;
            let record = self
                .records
                .get_mut(&prefix)
                .expect("rescan target remains registered");
            if record.refresh_pending.is_none() {
                self.refresh_queued[slot] += 1;
            }
            record.refresh_pending = Some(record.refresh_pending.map_or(mode, |old| old.max(mode)));
            self.refresh[slot]
                .as_mut()
                .expect("active rescan cursor")
                .progress = RefreshProgress::Scanning(Some(prefix));
        }
        Ok(())
    }
    /// 已完整送入当前 TCP 的宣告数；不代表远端确认。
    pub fn len(&self) -> usize {
        self.advertised
    }
    /// 当前 TCP 是否已完整接受此网段的宣告；撤回完整送出后为 false，不代表远端确认。
    /// 只读实际发送记录，不因排队/编码成功而提前报告，不保留另一份观测状态或分配。
    pub fn is_advertised(&self, prefix: Prefix) -> bool {
        self.records
            .get(&prefix)
            .is_some_and(|record| record.route.is_some())
    }
    /// 最近因当前方向容量/AS 编码无法发布的目标和原因，不代表会话断开。
    pub fn rejected_route(&self) -> Option<(Prefix, PacketError)> {
        self.rejected
    }
    /// 当前连接没有实际已发送的路由。
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// 登记真实选择变化；同前缀只保留一个截止，准备时再读取最新 Loc-RIB。
    /// # Errors
    /// 索引预留失败；调用者须结束该域，不能跳过更新继续把旧发送记录当作准确状态。
    pub fn changed(&mut self, prefix: Prefix, now: u64) -> Result<(), ExportError> {
        if !(router_protocol::tracking::IpPrefix {
            network: prefix.address,
            length: prefix.length,
        })
        .valid()
        {
            return Err(ExportError::Invalid);
        }
        if !self.records.contains_key(&prefix) {
            self.records
                .try_insert(
                    prefix,
                    Record {
                        route: None,
                        sent_at: None,
                        pending_at: None,
                        refresh_pending: None,
                    },
                )
                .map_err(|_| ExportError::NoMemory)?;
        }
        let record = self
            .records
            .get_mut(&prefix)
            .expect("registered output record");
        if record.pending_at == Some(now) {
            return Ok(());
        }
        self.ready
            .try_insert((now, prefix), ())
            .map_err(|_| ExportError::NoMemory)?;
        if let Some(old) = record.pending_at.replace(now) {
            self.ready.remove(&(old, prefix));
        }
        Ok(())
    }
    /// 最早尚未处理的截止；已有帧在途时必须等发送就绪，不用过期截止忙轮询。
    pub fn next_deadline(&self) -> Option<u64> {
        if self.flight.is_some() {
            return None;
        }
        self.ready
            .iter()
            .next()
            .map(|((at, _), ())| *at)
            .or_else(|| self.end_pending.iter().any(|pending| *pending).then_some(0))
            .into_iter()
            .chain(
                (self.refresh_requests.contains(&true)
                    || self.refresh_boundary().is_some()
                    || self.refresh_queued.iter().sum::<usize>() < REFRESH_BATCH
                        && (self.refresh_scanning(0) || self.refresh_scanning(1)))
                .then_some(0),
            )
            .min()
    }
    /// 报文在途时的真实类型。边界与 UPDATE 由同一 TCP 写入顺序确认。
    pub fn prepared_kind(&self) -> Option<packet_wire::bgp::Kind> {
        self.flight.as_ref().map(|flight| match flight {
            Flight::RefreshRequest(_) | Flight::RefreshBoundary { .. } => {
                packet_wire::bgp::Kind::RouteRefresh
            }
            Flight::Routes { .. } | Flight::End(_) => packet_wire::bgp::Kind::Update,
        })
    }
    fn prepare_refresh_frame(
        &mut self,
        family: Family,
        subtype: u8,
        flight: Flight,
    ) -> Result<Option<Vec<u8>>, ExportError> {
        let frame = packet_wire::bgp::route_refresh(family, subtype);
        let mut message = Vec::new();
        message
            .try_reserve_exact(frame.len())
            .map_err(|_| ExportError::NoMemory)?;
        message.extend_from_slice(&frame);
        self.flight = Some(flight);
        Ok(Some(message))
    }
    /// 将可发且实际出口属性相同的目标合入一个完整 UPDATE；每轮有界处理，不额外等待凑批。
    /// None 也可能表示消费了无变化工作或延后了重复宣告。
    /// # Parameters
    /// domain 只读已确认选择；policy 缺省拒绝；context/now 来自同一实际连接与单调时钟。
    /// # Returns
    /// 完整最终报文；调用者持有其部分发送偏移，并且只在整帧送入 TCP 后调用 sent。
    /// # Errors
    /// 内存不足、非法连接投影或字段不能编码；失败不把新值记作已发布。
    pub fn prepare(
        &mut self,
        domain: &mut RoutingDomain,
        policy: Option<&ExportPolicy>,
        context: ExportContext,
        now: u64,
    ) -> Result<Option<Vec<u8>>, ExportError> {
        if self.flight.is_some() {
            return Ok(None);
        }
        if let Some(slot) = self.refresh_requests.iter().position(|&queued| queued) {
            let family = if slot == 0 {
                Family::Ipv4
            } else {
                Family::Ipv6
            };
            return self.prepare_refresh_frame(family, 0, Flight::RefreshRequest(family));
        }
        self.queue_refresh(domain, now)?;
        if let Some((family, begin)) = self.refresh_boundary() {
            return self.prepare_refresh_frame(
                family,
                if begin { 1 } else { 2 },
                Flight::RefreshBoundary { family, begin },
            );
        }
        let mut prefixes = Vec::new();
        let mut advertised = None;
        let mut nlri_size = 0;
        let mut attributes_length = 0;
        let mut batch_mp = false;
        let mut after = None;
        for _ in 0..EXPORT_BATCH {
            let key = after
                .map_or_else(
                    || self.ready.first_key_value(),
                    |key| self.ready.successor(&key),
                )
                .map(|(key, ())| *key);
            let Some((at, prefix)) = key else { break };
            if at > now {
                break;
            }
            after = key;
            let mut next = match projection(domain, policy, context, prefix) {
                Err(ExportError::Packet(PacketError::Unsupported)) => {
                    self.rejected = Some((prefix, PacketError::Unsupported));
                    None
                }
                result => result?,
            };
            let record = self
                .records
                .get(&prefix)
                .expect("deadline belongs to output record");
            let due = record
                .sent_at
                .map(|sent| sent.saturating_add(context.interval_ns));
            if record
                .refresh_pending
                .is_none_or(|mode| mode == RefreshMode::Reconsider)
                && same_output(record.route.as_deref(), next.as_ref(), context.target.kind)
                && (next.is_some() || due.is_none_or(|due| due <= now))
            {
                self.retire_work(at, prefix);
                continue;
            }
            if let Some(due) = due
                && due > now
            {
                self.ready
                    .try_insert((due, prefix), ())
                    .map_err(|_| ExportError::NoMemory)?;
                self.ready.remove(&(at, prefix));
                self.records
                    .get_mut(&prefix)
                    .expect("output record exists")
                    .pending_at = Some(due);
                continue;
            }
            if prefixes.first().is_some_and(|first: &FlightPrefix| {
                first.prefix.address.is_ipv4() != prefix.address.is_ipv4()
            }) || !prefixes.is_empty()
                && !same_output(next.as_ref(), advertised.as_ref(), context.target.kind)
            {
                // 目标间无发送顺序依赖；有界向前寻找同组，异组保持原截止，不增加全表分组索引。
                continue;
            }
            let size = 1 + usize::from(prefix.length).div_ceil(8);
            let family = if prefix.address.is_ipv4() {
                Family::Ipv4
            } else {
                Family::Ipv6
            };
            if prefixes.is_empty() {
                match wire::attributes_length(next.as_ref(), context) {
                    Ok(length) => attributes_length = length,
                    Err(ExportError::Packet(PacketError::Capacity)) => {
                        self.rejected = Some((prefix, PacketError::Capacity));
                        next = None;
                        attributes_length = 0;
                    }
                    Err(error) => return Err(error),
                }
            }
            let uses_mp = |next: Option<&Advertisement>| {
                family == Family::Ipv6
                    || next
                        .or(record.route.as_deref())
                        .is_some_and(|route| route.next_hop.address.is_ipv6())
            };
            let mut multiprotocol = uses_mp(next.as_ref());
            if !prefixes.is_empty() && batch_mp != multiprotocol {
                // 撤回沿用上次实际发布的编码，两种 IPv4 撤回不能合为同一个 NLRI 列表。
                continue;
            }
            let length = wire::message_length(
                attributes_length,
                next.as_ref(),
                multiprotocol,
                nlri_size + size,
            )?;
            if length > packet_wire::bgp::STANDARD_LIMIT {
                if !prefixes.is_empty() {
                    break;
                }
                self.rejected = Some((prefix, PacketError::Capacity));
                next = None;
                attributes_length = 0;
                multiprotocol = uses_mp(None);
            }
            if next.is_none()
                && self
                    .records
                    .get(&prefix)
                    .is_some_and(|record| record.route.is_none())
            {
                self.retire_work(at, prefix);
                continue;
            }
            // 1. 编码前预留整批完成所需资源；索引/重发义务此时尚未消费，失败仍可安全重试。
            prefixes.try_reserve(1).map_err(|_| ExportError::NoMemory)?;
            let expiry = if next.is_none() && context.interval_ns != 0 {
                Some(FallibleMap::try_reserve_node().map_err(|_| ExportError::NoMemory)?)
            } else {
                None
            };
            prefixes.push(FlightPrefix { prefix, expiry });
            if prefixes.len() == 1 {
                advertised = next;
                batch_mp = multiprotocol;
            }
            nlri_size += size;
        }
        if !prefixes.is_empty() {
            let message = wire::message(
                prefixes.iter().map(|target| target.prefix),
                advertised.as_ref(),
                context,
                if prefixes[0].prefix.address.is_ipv4() {
                    Family::Ipv4
                } else {
                    Family::Ipv6
                },
                batch_mp,
            )?;
            // 只有完整编码及本批资源都已预留才分配共享投影；失败保留原工作，不在 sent 中分配。
            let advertised = advertised
                .map(Rc::try_new)
                .transpose()
                .map_err(|_| ExportError::NoMemory)?;
            // 2. 完整报文成功后才消费所含目标；编码后新到的 changed/refresh 仍可登记在同一记录上。
            for target in &prefixes {
                let record = self
                    .records
                    .get_mut(&target.prefix)
                    .expect("staged output record exists");
                let at = record
                    .pending_at
                    .take()
                    .expect("staged work remains pending");
                self.ready.remove(&(at, target.prefix));
                if record.refresh_pending.take().is_some() {
                    self.refresh_queued[usize::from(target.prefix.address.is_ipv6())] -= 1;
                }
            }
            self.flight = Some(Flight::Routes {
                prefixes,
                route: advertised,
                interval: context.interval_ns,
            });
            Ok(Some(message))
        } else if self.ready.is_empty()
            && self.refresh.iter().all(Option::is_none)
            && let Some(slot) = self.end_pending.iter().position(|pending| *pending)
        {
            let family = if slot == 0 {
                Family::Ipv4
            } else {
                Family::Ipv6
            };
            let message = wire::message(
                core::iter::empty(),
                None,
                context,
                family,
                family == Family::Ipv6,
            )?;
            self.flight = Some(Flight::End(family));
            Ok(Some(message))
        } else {
            Ok(None)
        }
    }
    fn retire_work(&mut self, at: u64, prefix: Prefix) {
        self.ready.remove(&(at, prefix));
        let record = self.records.get_mut(&prefix).expect("output record exists");
        record.pending_at = None;
        if record.refresh_pending.take().is_some() {
            self.refresh_queued[usize::from(prefix.address.is_ipv6())] -= 1;
        }
        if record.route.is_none() {
            self.records.remove(&prefix);
        }
    }
    /// 一帧已完整送入 TCP；无分配更新发送记录，保留编码后新到的工作。
    pub fn sent(&mut self, now: u64) {
        match self.flight.take() {
            Some(Flight::Routes {
                prefixes,
                route,
                interval,
            }) => {
                // 3. 整帧只确认一次；每个目标保留独立间隔与期间新到的工作，完成阶段不分配。
                for FlightPrefix { prefix, expiry } in prefixes {
                    let record = self
                        .records
                        .get_mut(&prefix)
                        .expect("in-flight record remains registered");
                    match (record.route.is_some(), route.is_some()) {
                        (false, true) => self.advertised += 1,
                        (true, false) => self.advertised -= 1,
                        _ => {}
                    }
                    record.sent_at = Some(now);
                    record.route = route.clone();
                    if record.route.is_none() && record.pending_at.is_none() {
                        if let Some(slot) = expiry {
                            let due = now.saturating_add(interval);
                            self.ready.commit_vacant(slot.fill((due, prefix), ()));
                            record.pending_at = Some(due);
                        } else {
                            self.records.remove(&prefix);
                        }
                    }
                }
            }
            Some(Flight::End(family)) => {
                self.end_pending[usize::from(family == Family::Ipv6)] = false
            }
            Some(Flight::RefreshRequest(family)) => {
                self.refresh_requests[usize::from(family == Family::Ipv6)] = false;
            }
            Some(Flight::RefreshBoundary { family, begin }) => {
                let refresh = &mut self.refresh[usize::from(family == Family::Ipv6)];
                let active = refresh.as_mut().expect("boundary retains its refresh");
                if begin {
                    active.progress = RefreshProgress::Scanning(None);
                } else {
                    *refresh = active.again.map(Refresh::new);
                }
            }
            None => {}
        }
    }
}
fn projection(
    domain: &mut RoutingDomain,
    policy: Option<&ExportPolicy>,
    context: ExportContext,
    prefix: Prefix,
) -> Result<Option<Advertisement>, ExportError> {
    let Some(policy) = policy else {
        return Ok(None);
    };
    let Some(route) = domain.export_route(prefix) else {
        return Ok(None);
    };
    let local = route.peer.is_none();
    let origin = route.peer.copied();
    let attributes = route.attributes.clone();
    let original_hop = route.next_hop;
    let original_preference = route.preference;
    let external = context.target.kind == PeerKind::External;
    if origin.is_some_and(|origin| {
        origin.id == context.target.id
            || context.target.kind == PeerKind::Internal
                && origin.kind == PeerKind::Internal
                && (context.cluster_id.is_none()
                    || !origin.route_reflector_client && !context.target.route_reflector_client)
    }) || match context.target.kind {
        PeerKind::External => attributes.path().contains(context.target.asn),
        PeerKind::Confederation => attributes.path().segments().any(|(kind, asns)| {
            matches!(
                kind,
                packet_wire::bgp::attributes::SegmentKind::ConfederationSequence
                    | packet_wire::bgp::attributes::SegmentKind::ConfederationSet
            ) && asns.contains(&context.target.asn)
        }),
        // 同 AS 的去环已经由域准入检查；联盟内成员号码出现在普通路径中并不代表成员环路。
        PeerKind::Internal => false,
    } || restricted(&attributes, context.target.kind)
    {
        return Ok(None);
    }
    let reflected_originator = if let Some(origin) = origin.filter(|origin| {
        context.target.kind == PeerKind::Internal && origin.kind == PeerKind::Internal
    }) {
        let identifier = attributes.originator_id().unwrap_or(origin.identifier);
        if identifier == context.target.identifier
            || context
                .cluster_id
                .is_some_and(|id| attributes.cluster_list().contains(&id))
        {
            return Ok(None);
        }
        Some(identifier)
    } else {
        None
    };
    let rule = action(&policy.rules, prefix, &attributes, original_preference);
    let (preference, rewritten) = match rule {
        Some(RouteAction::Deny) => return Ok(None),
        Some(RouteAction::Permit {
            preference,
            next_hop,
            ..
        }) => (*preference, *next_hop),
        None if policy.default == Disposition::Permit => (None, None),
        None => return Ok(None),
    };
    if if prefix.address.is_ipv4() {
        !context.ipv4_unicast
    } else {
        !context.ipv6_unicast
    } {
        return Ok(None);
    }
    let next_hop_self = rewritten.is_none() && (external || local);
    let next_hop = rewritten.unwrap_or(if next_hop_self {
        context.local_address
    } else {
        original_hop.address
    });
    if prefix.address.is_ipv4() && next_hop.is_ipv6() && !context.send_ipv4_via_ipv6
        || prefix.address.is_ipv6() && next_hop.is_ipv4()
    {
        return Err(PacketError::Unsupported.into());
    }
    if context.local_as == 0
        || next_hop.is_unspecified()
        || next_hop.is_multicast()
        || matches!(next_hop, IpAddr::V4(address) if address.is_broadcast())
        || matches!(next_hop, IpAddr::V6(address) if address.is_unicast_link_local() && !next_hop_self || address.to_ipv4_mapped().is_some())
    {
        return Ok(None);
    }
    let next_hop = domain.export_next_hop(
        context.target,
        next_hop,
        origin.map(|peer| (peer, original_hop)),
        next_hop_self,
    )?;
    let changes = AttributeChanges::permitted(rule);
    // 未显式设置的外部 MED 必须丢弃；本地出口 Set 是新的本地声明，不转发上游非传递属性。
    let changes = AttributeChanges {
        med: changes
            .med
            .or_else(|| (external && !local).then_some(MedAction::Remove)),
        ..changes
    };
    let attributes = domain
        .project_attributes(&attributes, changes)
        .map_err(|_| ExportError::NoMemory)?;
    // OLD 方向不能用 AS4_PATH 修复联盟成员 AS；逐目标拒绝并撤回旧宣告，不能把整个域当作损坏。
    if context.width == AsnWidth::Two
        && context.target.kind != PeerKind::External
        && (context.target.kind == PeerKind::Confederation
            && context.local_as > u32::from(u16::MAX)
            || attributes.path().segments().any(|(kind, asns)| {
                matches!(
                    kind,
                    packet_wire::bgp::attributes::SegmentKind::ConfederationSequence
                        | packet_wire::bgp::attributes::SegmentKind::ConfederationSet
                ) && asns.iter().any(|asn| *asn > u32::from(u16::MAX))
            }))
    {
        return Err(PacketError::Unsupported.into());
    }
    // 出口新加的保留团体约束接收邻居的后续传播；本次限制已在改写前按 Loc-RIB 检查。
    Ok(Some(Advertisement {
        attributes,
        next_hop,
        reflected_originator,
        preference: (!external).then_some(preference.unwrap_or(original_preference)),
    }))
}

fn restricted(attributes: &RouteAttributes, kind: PeerKind) -> bool {
    let communities = attributes.communities();
    communities.binary_search(&0xffff_ff02).is_ok()
        || kind == PeerKind::External && communities.binary_search(&0xffff_ff01).is_ok()
        || kind != PeerKind::Internal && communities.binary_search(&0xffff_ff03).is_ok()
}
