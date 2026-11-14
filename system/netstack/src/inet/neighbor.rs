//! 以太网邻接的解析、可达性、限速与时钟。ARP 和 ND 各自负责报文与学习准入。
use fallible_tree::{FallibleMap, NodeSlot};
use network_protocol::{NeighborState, SocketError};

// 与固定 Linux 默认 gc_thresh3 一致的邻居硬上限；仅在压力下回收未被排队报文引用的 STALE。
const NEIGHBORS: usize = 1024;
const WORK: usize = 8;
/// 同一次有界推进的完成事实；NET 在邻居借用结束后按当前路由处理 socket 失败，不缓存每流网关。
pub(in crate::inet) struct Progress<A> {
    pub(in crate::inet) backlog: bool,
    pub(in crate::inet) failed: [Option<A>; WORK],
    pub(in crate::inet) reachability_changed: bool,
}
const MAX_SOLICIT: u8 = 3;
const DELAY_PROBE: u64 = 5000;
const ROUTER_REPROBE_INTERVAL: u64 = 60_000;
const RANDOMIZE_REACHABLE_INTERVAL: u64 = 3 * 60 * 60 * 1000;

#[derive(Clone, Copy)]
enum State {
    Incomplete(u8),
    Reachable,
    Stale,
    Delay,
    Probe(u8),
    Failed { retry_at: u64 },
}
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Wake {
    At(u64),
    Writable,
}
enum Clock<A> {
    Queued(Wake),
    Idle(NodeSlot<(Wake, A), ()>),
}
struct Neighbor<A> {
    mac: Option<[u8; 6]>,
    state: State,
    used: u64,
    // OWNER: 有效 NA 的 Router 位；与 MAC/NUD 一同更新，不能在展示层猜测路由器身份。
    is_router: bool,
    // None 只在本 owner 的无分配索引交换内存在，任何可观察状态都必须有一个完整 clock。
    clock: Option<Clock<A>>,
}

impl<A> Default for Progress<A> {
    fn default() -> Self {
        Self {
            backlog: false,
            failed: core::array::from_fn(|_| None),
            reachability_changed: false,
        }
    }
}

pub(super) struct NeighborCache<A> {
    entries: FallibleMap<A, Neighbor<A>>,
    // 每个邻居预留且只持有一个 clock node；时钟树仅为索引，不持第二份协议状态。
    deadlines: FallibleMap<(Wake, A), ()>,
    base_reachable_ms: u32,
    retransmit_ms: u32,
    // 同链路确认复用同一随机窗口。首次确认、参数变化或三小时后再抽样，避免每条 NA 调用随机源。
    reachable_ms: u64,
    randomize_at: u64,
    // 批内合并进入/离开 Failed 的通知。RX、探测和压力回收都会改变该集合，接口轮询必须完整交付。
    reachability_changed: bool,
}
impl<A: Copy + Ord> NeighborCache<A> {
    pub(super) fn new() -> Self {
        Self {
            entries: FallibleMap::new(),
            deadlines: FallibleMap::new(),
            base_reachable_ms: 30_000,
            retransmit_ms: 1000,
            reachable_ms: 0,
            randomize_at: 0,
            reachability_changed: false,
        }
    }
    /// 接受通告指定的毫秒参数。零表示省略，沿用现值。已排队的截止时刻保持有效。
    pub(super) fn apply_timers(&mut self, reachable_ms: u32, retransmit_ms: u32) {
        if reachable_ms != 0 && self.base_reachable_ms != reachable_ms.max(100) {
            self.base_reachable_ms = reachable_ms.max(100);
            self.randomize_at = 0;
        }
        if retransmit_ms != 0 {
            self.retransmit_ms = retransmit_ms.max(10);
        }
    }

    pub(super) fn retransmit_ms(&self) -> u64 {
        u64::from(self.retransmit_ms)
    }
    /// 借用同一 owner 的事实，不延长邻居寿命、推进 NUD 或复制时钟索引。
    pub(super) fn rows(
        &self,
        interface_index: u32,
        after: Option<A>,
    ) -> impl Iterator<Item = network_protocol::Neighbor> + Clone
    where
        A: Into<core::net::IpAddr>,
    {
        let rows = match after {
            Some(address) => self.entries.iter_after(&address),
            None => self.entries.iter(),
        };
        rows.map(move |(&address, entry)| {
            let (state, probes) = match entry.state {
                State::Incomplete(probes) => (NeighborState::Incomplete, probes),
                State::Reachable => (NeighborState::Reachable, 0),
                State::Stale => (NeighborState::Stale, 0),
                State::Delay => (NeighborState::Delay, 0),
                State::Probe(probes) => (NeighborState::Probe, probes),
                State::Failed { .. } => (NeighborState::Failed, 0),
            };
            network_protocol::Neighbor {
                interface_index,
                address: address.into(),
                hardware_address: (!matches!(entry.state, State::Failed { .. }))
                    .then_some(entry.mac)
                    .flatten(),
                state,
                is_router: entry.is_router,
                probes,
            }
        })
    }
    pub(super) fn take_reachability_change(&mut self) -> bool {
        core::mem::take(&mut self.reachability_changed)
    }
    fn schedule(&mut self, address: A, wake: Option<Wake>) {
        let clock = self
            .entries
            .get_mut(&address)
            .expect("neighbor owns its clock")
            .clock
            .take()
            .expect("clock exchange is not reentrant");
        let storage = match clock {
            Clock::Idle(storage) => storage,
            Clock::Queued(previous) => {
                self.deadlines
                    .take_entry(&(previous, address))
                    .expect("queued neighbor clock missing")
                    .into_slot()
                    .0
            }
        };
        let clock = if let Some(wake) = wake {
            self.deadlines
                .commit_vacant(storage.fill((wake, address), ()));
            Clock::Queued(wake)
        } else {
            Clock::Idle(storage)
        };
        self.entries
            .get_mut(&address)
            .expect("clock exchange retains neighbor")
            .clock = Some(clock);
    }
    fn remove(&mut self, address: A) {
        let neighbor = self
            .entries
            .remove(&address)
            .expect("neighbor retired once");
        self.reachability_changed |= matches!(neighbor.state, State::Failed { .. });
        if let Some(Clock::Queued(wake)) = neighbor.clock {
            self.deadlines
                .remove(&(wake, address))
                .expect("queued neighbor clock missing");
        }
    }
    pub(super) fn ensure(&mut self, address: A, now: u64, pinned: impl Fn(A) -> bool) -> bool {
        if self.entries.contains_key(&address) {
            return true;
        }
        let victim = if self.entries.len() >= NEIGHBORS {
            let candidate = self
                .entries
                .iter()
                .filter(|(address, entry)| {
                    // 已排队的失败网关承担复探义务。回收它会丢掉失联事实和唯一的恢复时钟。
                    (matches!(entry.state, State::Stale)
                        || matches!(entry.state, State::Failed { .. })
                            && matches!(entry.clock, Some(Clock::Idle(_))))
                        && !pinned(**address)
                })
                .min_by_key(|(_, entry)| entry.used)
                .map(|(&address, _)| address);
            let Some(candidate) = candidate else {
                return false;
            };
            Some(candidate)
        } else {
            None
        };
        // 两份索引 storage 全部准备成功才回收/发布；任一 OOM 保持已有邻居和时钟完整。
        let Ok(clock) = FallibleMap::try_reserve_node() else {
            return false;
        };
        let Ok(entry) = FallibleMap::try_prepare(
            address,
            Neighbor {
                mac: None,
                state: State::Incomplete(0),
                used: now,
                is_router: false,
                clock: Some(Clock::Idle(clock)),
            },
        ) else {
            return false;
        };
        if let Some(victim) = victim {
            self.remove(victim);
        }
        self.entries.commit_vacant(entry);
        self.schedule(address, Some(Wake::At(now)));
        true
    }
    /// 从调用方已校验的邻居请求或通告学习地址，未验证可达性时标记为 Stale。
    pub(super) fn learn(
        &mut self,
        address: A,
        mac: [u8; 6],
        now: u64,
        pinned: impl Fn(A) -> bool,
    ) -> bool {
        if !self.ensure(address, now, pinned) {
            return false;
        }
        let entry = self.entries.get_mut(&address).expect("prepared neighbor");
        if entry.mac != Some(mac) {
            self.reachability_changed |= matches!(entry.state, State::Failed { .. });
            entry.mac = Some(mac);
            entry.state = State::Stale;
            self.schedule(address, None);
        }
        true
    }
    pub(super) fn mark_router(&mut self, address: A) {
        if let Some(entry) = self.entries.get_mut(&address) {
            entry.is_router = true;
        }
    }
    /// 第一位区分已知失败，第二位用于同优先级路径的邻居解析状态。
    pub(super) fn router_rank(&self, address: A) -> (bool, bool) {
        self.entries.get(&address).map_or((true, false), |entry| {
            let available = !matches!(entry.state, State::Failed { .. });
            (available, available && entry.mac.is_some())
        })
    }
    pub(super) fn router_probe_due(&self, address: A, now: u64) -> bool {
        self.entries.get(&address).is_some_and(|entry| {
            matches!(entry.state, State::Failed { retry_at } if retry_at <= now)
                && matches!(entry.clock, Some(Clock::Idle(_)))
        })
    }

    /// 保持 Failed，直至收到有效回复。只在本次真实发送需要该路径时排一个有界探测。
    pub(super) fn request_router_probe(&mut self, address: A, now: u64) {
        if self.router_probe_due(address, now) {
            self.schedule(address, Some(Wake::At(now)));
        }
    }
    pub(super) fn monitor_failed_gateway(&mut self, address: A, enable: bool) {
        let Some(entry) = self.entries.get(&address) else {
            return;
        };
        if let State::Failed { retry_at } = entry.state {
            if enable && matches!(entry.clock, Some(Clock::Idle(_))) {
                self.schedule(address, Some(Wake::At(retry_at)));
            } else if !enable {
                self.schedule(address, None);
            }
        }
    }
    /// 应用已校验的链路层确认。调用者仍负责协议专属的地址和通告语义。
    pub(super) fn confirm(
        &mut self,
        address: A,
        mac: Option<[u8; 6]>,
        solicited: bool,
        override_mac: bool,
        router: bool,
        now: u64,
    ) -> bool {
        let Some(entry) = self.entries.get_mut(&address) else {
            return false;
        };
        let was_failed = matches!(entry.state, State::Failed { .. });
        let changed = mac.is_some() && mac != entry.mac;
        if matches!(entry.state, State::Incomplete(_))
            || matches!(entry.state, State::Failed { .. }) && entry.mac.is_none()
        {
            let Some(mac) = mac else {
                return false;
            };
            entry.mac = Some(mac);
        } else if changed && !override_mac {
            if matches!(entry.state, State::Reachable) {
                entry.state = State::Stale;
                self.schedule(address, None);
            }
            return false;
        } else if let Some(mac) = mac {
            entry.mac = Some(mac);
        }
        entry.is_router = router;
        if solicited {
            entry.state = State::Reachable;
            if now >= self.randomize_at {
                let base = u64::from(self.base_reachable_ms);
                self.reachable_ms = base / 2 + super::multicast::random_delay(base);
                self.randomize_at = now.saturating_add(RANDOMIZE_REACHABLE_INTERVAL);
            }
            self.schedule(
                address,
                Some(Wake::At(now.saturating_add(self.reachable_ms))),
            );
        } else if changed {
            entry.state = State::Stale;
            self.schedule(address, None);
        }
        self.reachability_changed |= was_failed && (solicited || changed);
        true
    }
    pub(super) fn mac(&mut self, address: A, now: u64) -> Option<[u8; 6]> {
        let entry = self.entries.get_mut(&address)?;
        if matches!(entry.state, State::Failed { .. }) {
            return None;
        }
        let mac = entry.mac?;
        entry.used = now;
        // 时钟检查与 RX 可能同轮到达；过期 REACHABLE 的首次使用也必须进入 DELAY，不能漏掉 NUD。
        let expired = matches!(entry.state, State::Reachable)
            && matches!(entry.clock.as_ref(), Some(Clock::Queued(Wake::At(at))) if *at <= now);
        if matches!(entry.state, State::Stale) || expired {
            entry.state = State::Delay;
            self.schedule(address, Some(Wake::At(now.saturating_add(DELAY_PROBE))));
        }
        Some(mac)
    }
    /// UDP 保留自己的载荷，仅在本 owner 创建解析义务；不重复捕获 packet 或猜测 MAC。
    pub(super) fn resolve(
        &mut self,
        address: A,
        now: u64,
        pinned: impl Fn(A) -> bool,
    ) -> Result<Option<[u8; 6]>, SocketError> {
        if let Some(mac) = self.mac(address, now) {
            return Ok(Some(mac));
        }
        self.request_router_probe(address, now);
        if !self.ensure(address, now, pinned) {
            return Err(SocketError::NoMemory);
        }
        Ok(None)
    }

    /// 数据流量只能延长同一对端现存且匹配的映射。
    pub(super) fn confirm_existing(&mut self, address: A, mac: [u8; 6], now: u64) {
        if self.entries.get(&address).is_some_and(|entry| {
            entry.mac == Some(mac)
                && !matches!(entry.state, State::Failed { .. })
                && !matches!(entry.clock, Some(Clock::Queued(Wake::At(at)))
                    if matches!(entry.state, State::Reachable)
                        && at > now.saturating_add(self.reachable_ms / 2))
        }) {
            self.confirm(address, Some(mac), true, false, false, now);
        }
    }
    pub(super) fn due(&self, now: u64) -> bool {
        self.next_deadline().is_some_and(|at| at <= now)
    }

    pub(super) fn next_deadline(&self) -> Option<u64> {
        self.deadlines
            .first_key_value()
            .and_then(|((wake, _), _)| match wake {
                Wake::At(at) => Some(*at),
                _ => None,
            })
    }
    /// 地址解析/NUD 共享八项时钟预算，NIC 背压进入独立 writable 索引，不忙轮询也不全表扫描。
    pub(super) fn poll(
        &mut self,
        first: A,
        now: u64,
        mut send: impl FnMut(A, Option<[u8; 6]>) -> bool,
        is_gateway: impl Fn(A) -> bool,
        monitored_gateway: impl Fn(A) -> bool,
    ) -> Progress<A> {
        let mut progress = Progress::default();
        // 只捕获本轮开始时的 writable 身份，新失败不能在同一轮反复尝试。
        let initial: [Option<A>; WORK] = {
            let mut waiting = self.deadlines.iter_from(&(Wake::Writable, first));
            core::array::from_fn(|_| waiting.next().map(|((_, address), _)| *address))
        };
        let mut writable = initial.into_iter().flatten();
        let mut productive = false;
        for slot in &mut progress.failed {
            let due = self
                .deadlines
                .first_key_value()
                .and_then(|(&(wake, address), _)| {
                    matches!(wake, Wake::At(at) if at <= now).then_some(address)
                });
            let next = due.or_else(|| writable.next());
            let Some(address) = next else {
                break;
            };
            let state = self
                .entries
                .get(&address)
                .expect("timer refers to actual neighbor")
                .state;
            match state {
                State::Reachable => {
                    self.entries.get_mut(&address).unwrap().state = State::Stale;
                    self.schedule(address, None);
                    productive = true;
                }
                State::Incomplete(MAX_SOLICIT..) | State::Probe(MAX_SOLICIT..) => {
                    // 当前路径或待复探依赖引用的网关保留失败事实，供下一次选路使用。
                    *slot = Some(address);
                    let monitored = monitored_gateway(address);
                    if monitored || is_gateway(address) {
                        let entry = self
                            .entries
                            .get_mut(&address)
                            .expect("live router neighbor");
                        entry.state = State::Failed { retry_at: now };
                        self.reachability_changed = true;
                        self.schedule(address, monitored.then_some(Wake::At(now)));
                    } else {
                        self.remove(address);
                    }
                    productive = true;
                }
                State::Failed { .. } => {
                    let monitored = monitored_gateway(address);
                    if !monitored && !is_gateway(address) {
                        self.remove(address);
                        *slot = Some(address);
                        productive = true;
                        continue;
                    }
                    let accepted = send(address, None);
                    let wake = if accepted {
                        let retry_at = now.saturating_add(ROUTER_REPROBE_INTERVAL);
                        self.entries
                            .get_mut(&address)
                            .expect("live router neighbor")
                            .state = State::Failed { retry_at };
                        monitored.then_some(Wake::At(retry_at))
                    } else {
                        Some(Wake::Writable)
                    };
                    productive |= accepted;
                    self.schedule(address, wake);
                }
                State::Incomplete(_) | State::Probe(_) | State::Delay => {
                    let sent = match state {
                        State::Incomplete(sent) | State::Probe(sent) => sent,
                        State::Delay => 0,
                        _ => unreachable!("probe state checked"),
                    };
                    let multicast = matches!(state, State::Incomplete(_));
                    let mac = (!multicast).then(|| {
                        self.entries
                            .get(&address)
                            .unwrap()
                            .mac
                            .expect("unicast NUD has a MAC")
                    });
                    let accepted = send(address, mac);
                    productive |= accepted;
                    self.entries.get_mut(&address).unwrap().state = if multicast {
                        State::Incomplete(sent + u8::from(accepted))
                    } else {
                        State::Probe(sent + u8::from(accepted))
                    };
                    self.schedule(
                        address,
                        Some(if accepted {
                            Wake::At(now.saturating_add(self.retransmit_ms()))
                        } else {
                            Wake::Writable
                        }),
                    );
                }
                State::Stale => unreachable!("STALE has no active timer"),
            }
        }
        progress.backlog = self.due(now)
            || productive && self.deadlines.ceiling(&(Wake::Writable, first)).is_some();
        progress.reachability_changed = self.take_reachability_change();
        progress
    }
}
