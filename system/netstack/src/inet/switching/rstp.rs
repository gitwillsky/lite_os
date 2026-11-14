//! 单棵 802.1D 生成树。控制决策与 FDB 变更在同一个 NET guard 内完成。
use super::NetworkInterface;
use alloc::vec::Vec;
use network_protocol::SocketError;
use packet_wire::{
    ethernet::EthernetHeader,
    spanning_tree::{self as wire, Bpdu, BridgeId, Configuration, Priority, Times},
};
use ::tcp::time::{Duration, Instant};

const ZERO: Instant = Instant::ZERO;
const MIGRATE: Duration = Duration::from_millis(3_000);
const RETRY: Duration = Duration::from_millis(100);
const TX_HOLD: u8 = 6;

fn duration(wire: u16) -> Duration {
    Duration::from_millis((u64::from(wire) * 1000).div_ceil(256))
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum State {
    Discarding,
    Learning,
    Forwarding,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    Disabled,
    Root,
    Designated,
    Alternate,
    Backup,
}

impl Role {
    fn selected(self) -> bool {
        matches!(self, Self::Root | Self::Designated)
    }
    fn flags(self) -> u8 {
        match self {
            Self::Disabled => 0,
            Self::Root => wire::ROOT,
            Self::Designated => wire::DESIGNATED,
            Self::Alternate | Self::Backup => wire::ALTERNATE,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Settings {
    index: u32,
    id: u16,
    cost: u32,
    edge: bool,
    point_to_point: bool,
}

#[derive(Clone, Copy)]
struct Received {
    message: Configuration,
    until: Instant,
}

struct Port {
    settings: Settings,
    active: bool,
    edge: bool,
    role: Role,
    state: State,
    transition: Instant,
    received: Option<Received>,
    // 迁移按链路处理。传统 BPDU 停止且迁移间隔已过后，收到 RST BPDU 可恢复快速模式。
    legacy_until: Option<Instant>,
    // 只有已成功提交的提议才能使对应的确认提前开启指定端口转发。
    proposal: Option<Priority>,
    // 已完成同步的选路向量。
    agreement: Option<Priority>,
    topology_ack: bool,
    topology_until: Instant,
    // 旧版根端口的 TCN 必须重发至对端确认，端口换代或角色变化时撤销。
    topology_pending: bool,
    transmit: Instant,
    hold_until: Instant,
    sent: u8,
}

impl Port {
    fn new(settings: Settings) -> Self {
        Self {
            settings,
            active: false,
            edge: settings.edge,
            role: Role::Disabled,
            state: State::Discarding,
            transition: ZERO,
            received: None,
            legacy_until: None,
            proposal: None,
            agreement: None,
            topology_ack: false,
            topology_until: ZERO,
            topology_pending: false,
            transmit: ZERO,
            hold_until: ZERO,
            sent: 0,
        }
    }

    fn rapid(&self) -> bool {
        self.legacy_until.is_none()
    }

    fn discard(&mut self, now: Instant, delay: u16) -> bool {
        let forwarding = self.state == State::Forwarding;
        self.state = State::Discarding;
        self.transition = now + duration(delay);
        self.proposal = None;
        self.agreement = None;
        self.topology_ack = false;
        self.topology_until = ZERO;
        self.topology_pending = false;
        self.transmit = now;
        forwarding
    }

    fn live(&self, now: Instant) -> Option<Configuration> {
        self.received
            .filter(|entry| entry.until > now)
            .map(|entry| entry.message)
    }

    fn control_due(&self, now: Instant) -> bool {
        self.control_deadline(now).is_some_and(|at| at <= now)
    }

    fn control_deadline(&self, now: Instant) -> Option<Instant> {
        (self.active
            && (self.role == Role::Designated
                || self.rapid() && self.agreement.is_some()
                || self.role == Role::Root
                    && (self.topology_pending || self.rapid() && self.topology_until > now)))
            .then_some(self.transmit)
    }

    fn mark_topology(&mut self, now: Instant, times: Times) {
        if self.topology_until > now {
            return;
        }
        self.topology_until = now
            + if self.rapid() {
                duration(times.hello) + Duration::from_millis(1_000)
            } else {
                self.topology_pending = self.role == Role::Root;
                duration(times.max_age) + duration(times.forward_delay)
            };
        self.transmit = now;
    }
}

fn interface(interfaces: &[NetworkInterface], index: u32) -> Option<&NetworkInterface> {
    NetworkInterface::position(interfaces, index)
        .map(|at| &interfaces[at])
}

/// 一座网桥的公共生成树。按接口排序，发送前应用端口状态变更。
pub(super) struct Tree {
    priority: u16,
    id: Option<BridgeId>,
    ports: Vec<Port>,
    root_priority: Priority,
    root_port: Option<usize>,
    times: Times,
    // 载波采样可能先退休端口，下一轮仍须重新选根并退休旧树的 FDB。
    link_retired: bool,
}

impl Tree {
    pub(super) fn new(priority: u16) -> Self {
        Self {
            priority,
            id: None,
            ports: Vec::new(),
            root_priority: Self::own_priority([0; 8]),
            root_port: None,
            times: Times::DEFAULT,
            link_retired: false,
        }
    }

    fn own_priority(id: BridgeId) -> Priority {
        Priority {
            root: id,
            cost: 0,
            bridge: id,
            port: 0,
        }
    }

    pub(super) fn push(
        &mut self,
        input: router_protocol::switching::Port<'_>,
    ) -> Result<usize, SocketError> {
        self.ports
            .try_reserve(1)
            .map_err(|_| SocketError::NoMemory)?;
        let at = self.ports.len();
        self.ports.push(Port::new(Settings {
            index: input.index,
            id: 0x8000 | input.number,
            cost: input.path_cost,
            edge: input.edge,
            point_to_point: input.point_to_point,
        }));
        Ok(at)
    }

    pub(super) fn same_config(&self, other: &Self) -> bool {
        self.priority == other.priority
            && self
                .ports
                .iter()
                .map(|port| port.settings)
                .eq(other.ports.iter().map(|port| port.settings))
    }

    /// 成员变更或回滚后从阻塞开始。原配置停用期间对端可能已重新选路。
    pub(super) fn activate(&mut self, interfaces: &[NetworkInterface]) {
        self.id = self
            .ports
            .iter()
            .filter_map(|port| interface(interfaces, port.settings.index))
            .map(|interface| interface.device.mac_address())
            .filter(|mac| mac[0] & 1 == 0 && *mac != [0; 6])
            .min()
            .map(|mac| {
                let mut id = [0; 8];
                id[..2].copy_from_slice(&self.priority.to_be_bytes());
                id[2..].copy_from_slice(&mac);
                id
            });
        self.root_priority = Self::own_priority(self.id.unwrap_or([0; 8]));
        self.root_port = None;
        self.times = Times::DEFAULT;
        self.link_retired = false;
        for port in &mut self.ports {
            *port = Port::new(port.settings);
        }
    }

    pub(super) fn state(&self, port: usize) -> State {
        self.ports[port].state
    }

    pub(super) fn next_deadline(&self, now: Instant) -> Option<Instant> {
        self.link_retired
            .then_some(now)
            .into_iter()
            .chain(
                self.ports
                    .iter()
                    .flat_map(|port| {
                        [
                            port.control_deadline(now),
                            port.received.map(|entry| entry.until),
                            (port.role.selected() && !port.edge && port.state != State::Forwarding)
                                .then_some(port.transition),
                        ]
                    })
                    .flatten(),
            )
            .min()
    }

    pub(super) fn forget(&mut self, port: usize) {
        let port = &mut self.ports[port];
        self.link_retired |= port.active;
        *port = Port::new(port.settings);
    }

    fn advertised(&self, port: usize) -> Priority {
        Priority {
            bridge: self.id.expect("active bridge"),
            port: self.ports[port].settings.id,
            ..self.root_priority
        }
    }

    fn reselect(&mut self, now: Instant) -> bool {
        let Some(id) = self.id else {
            return false;
        };
        let mut best = (Self::own_priority(id), 0);
        let mut root = None;
        for (at, port) in self
            .ports
            .iter()
            .enumerate()
            .filter(|(_, port)| port.active)
        {
            let Some(received) = port.live(now) else {
                continue;
            };
            if received.priority.bridge == id
                || received.times.age.saturating_add(256) >= received.times.max_age
            {
                continue;
            }
            let candidate = (
                Priority {
                    cost: received.priority.cost.saturating_add(port.settings.cost),
                    ..received.priority
                },
                port.settings.id,
            );
            if candidate < best {
                best = candidate;
                root = Some((
                    at,
                    Times {
                        age: received.times.age + 256,
                        ..received.times
                    },
                ));
            }
        }
        let selected = root.map(|(port, _)| port);
        let root_changed = self.root_priority != best.0 || self.root_port != selected;
        let times = root.map_or(Times::DEFAULT, |(_, times)| times);
        self.root_priority = best.0;
        self.root_port = selected;
        self.times = times;
        let mut flush = root_changed;
        for (at, port) in self.ports.iter_mut().enumerate() {
            let designated = Priority {
                bridge: id,
                port: port.settings.id,
                ..self.root_priority
            };
            let role = if !port.active {
                Role::Disabled
            } else if Some(at) == selected {
                Role::Root
            } else if let Some(message) = port
                .live(now)
                .filter(|message| message.priority <= designated)
            {
                if message.priority.bridge == id {
                    Role::Backup
                } else {
                    Role::Alternate
                }
            } else {
                Role::Designated
            };
            if port.role != role || root_changed && !port.edge {
                flush |= port.discard(now, times.forward_delay);
                port.role = role;
            }
            if port.active && port.edge && role == Role::Designated {
                port.state = State::Forwarding;
            }
        }
        flush
    }

    fn topology(&mut self, now: Instant, ingress: Option<usize>) {
        for (at, port) in self.ports.iter_mut().enumerate() {
            if port.role.selected() && !port.edge && Some(at) != ingress {
                port.mark_topology(now, self.times);
            }
        }
    }

    fn synchronize_root(&mut self, at: usize, vector: Priority, now: Instant) -> bool {
        let root = &self.ports[at];
        if root.state == State::Forwarding && root.agreement == Some(vector) {
            self.ports[at].transmit = now;
            return false;
        }
        // 先关闭其他非边缘指定路径，再允许根端口转发并发送确认。
        for (port_at, port) in self.ports.iter_mut().enumerate() {
            if port_at != at && port.role == Role::Designated && !port.edge {
                port.discard(now, self.times.forward_delay);
            }
        }
        let root = &mut self.ports[at];
        root.state = State::Forwarding;
        root.agreement = Some(vector);
        root.transmit = now;
        self.topology(now, None);
        true
    }

    /// 按载波、BPDU 寿命和转发延迟推进端口。返回是否需清除本桥学习项。
    pub(super) fn tick(&mut self, interfaces: &[NetworkInterface], now: Instant) -> bool {
        if self.id.is_none() {
            return false;
        }
        let previous_root = self.root_port;
        let mut changed = core::mem::take(&mut self.link_retired);
        let mut flush = changed;
        for port in &mut self.ports {
            let active = interface(interfaces, port.settings.index)
                .is_some_and(NetworkInterface::link_active);
            if port.active != active {
                flush |= port.state == State::Forwarding;
                *port = Port::new(port.settings);
                port.active = active;
                changed = true;
            }
            if port.received.is_some_and(|entry| entry.until <= now) {
                port.received = None;
                changed = true;
            }
        }
        if changed {
            flush |= self.reselect(now);
            if previous_root.is_some()
                && self.root_port != previous_root
                && let Some(at) = self.root_port
                && let Some(message) = self.ports[at].live(now)
                && self.ports[at].settings.point_to_point
                && self.ports[at].rapid()
                && message.rapid
                && message.flags & wire::FORWARDING != 0
            {
                flush |= self.synchronize_root(at, message.priority, now);
            }
        }
        let mut topology = false;
        for port in &mut self.ports {
            if !port.role.selected() || port.edge || port.transition > now {
                continue;
            }
            match port.state {
                State::Discarding => {
                    port.state = State::Learning;
                    port.transition = now + duration(self.times.forward_delay);
                }
                State::Learning => {
                    port.state = State::Forwarding;
                    port.proposal = None;
                    port.transmit = now;
                    topology = true;
                }
                State::Forwarding => {}
            }
        }
        if topology {
            self.topology(now, None);
        }
        topology || flush
    }

    /// 处理物理链路收到的 BPDU。返回是否需清除本桥学习项。
    pub(super) fn receive(&mut self, at: usize, bpdu: Bpdu, now: Instant) -> bool {
        if self.id.is_none() || !self.ports[at].active {
            return false;
        }
        let advertised = self.advertised(at);
        if matches!(bpdu, Bpdu::Configuration(message) if !message.rapid
            && message.priority.bridge == advertised.bridge && message.priority.port == advertised.port)
        {
            return false;
        }
        let port = &mut self.ports[at];
        let mut flush = false;
        if port.edge {
            port.edge = false;
            flush |= port.discard(now, self.times.forward_delay);
        }
        let rapid = matches!(bpdu, Bpdu::Configuration(message) if message.rapid);
        if !rapid {
            if port.rapid() {
                flush |= port.discard(now, self.times.forward_delay);
            }
            port.legacy_until = Some(now + MIGRATE);
        } else if let Some(until) = port.legacy_until {
            if now < until {
                return flush;
            }
            port.legacy_until = None;
            port.topology_pending = false;
            port.transmit = now;
        }
        let Bpdu::Configuration(message) = bpdu else {
            if port.role == Role::Designated {
                port.mark_topology(now, self.times);
                port.topology_ack = true;
                port.transmit = now;
                self.topology(now, Some(at));
                flush = true;
            }
            return flush;
        };
        let flags = message.flags;
        if !message.rapid && flags & wire::TOPOLOGY_ACK != 0 && Some(at) == self.root_port {
            port.topology_pending = false;
            port.topology_until = ZERO;
        }
        let role = flags & wire::ROLE_MASK;
        // 未知角色按配置 BPDU 参与选根，仅明确的指定端口可发起快速握手。
        let designated = !message.rapid || matches!(role, 0 | wire::DESIGNATED);
        if designated {
            let previous = port.live(now);
            if previous.is_none_or(|old| {
                message.priority <= old.priority
                    || (message.priority.bridge, message.priority.port)
                        == (old.priority.bridge, old.priority.port)
            }) {
                let lifetime = if message.rapid {
                    duration(message.times.hello) * 3
                } else {
                    duration(message.times.max_age - message.times.age)
                };
                port.received = Some(Received {
                    message,
                    until: now + lifetime,
                });
                if previous.is_none_or(|old| {
                    old.priority != message.priority || old.times != message.times
                }) {
                    flush |= self.reselect(now);
                }
            }
            let advertised = self.advertised(at);
            let port = &mut self.ports[at];
            if port.role == Role::Designated
                && message.priority > advertised
                && message.rapid
                && flags & wire::LEARNING != 0
            {
                flush |= port.discard(now, self.times.forward_delay);
            } else if role == wire::DESIGNATED
                && flags & wire::PROPOSAL != 0
                && port.settings.point_to_point
                && port
                    .live(now)
                    .is_some_and(|selected| selected.priority == message.priority)
            {
                match port.role {
                    Role::Root => flush |= self.synchronize_root(at, message.priority, now),
                    Role::Alternate | Role::Backup => {
                        port.agreement = Some(message.priority);
                        port.transmit = now;
                    }
                    _ => {}
                }
            }
        } else if message.rapid
            && matches!(role, wire::ROOT | wire::ALTERNATE)
            && flags & wire::AGREEMENT != 0
            && port.settings.point_to_point
            && port.role == Role::Designated
            && port.proposal == Some(advertised)
            && message.priority.root == advertised.root
            && message.priority >= advertised
            && message.priority.bridge != advertised.bridge
        {
            port.proposal = None;
            port.state = State::Forwarding;
            port.transmit = now;
            self.topology(now, None);
            flush = true;
        }
        if flags & wire::TOPOLOGY_CHANGE != 0 && self.ports[at].role.selected() {
            self.topology(now, Some(at));
            flush = true;
        }
        flush
    }

    /// 发送本轮到期的 BPDU。成功提交才计入端口发送限额并记录已发出的提议。
    pub(super) fn transmit(
        &mut self,
        interfaces: &[NetworkInterface],
        now: Instant,
        budget: &mut usize,
    ) {
        let Some(id) = self.id else {
            return;
        };
        for port in &mut self.ports {
            if *budget == 0 {
                break;
            }
            if !port.control_due(now) {
                continue;
            }
            let Some(interface) =
                interface(interfaces, port.settings.index).filter(|entry| entry.link_active())
            else {
                continue;
            };
            let rapid = port.rapid();
            let topology = port.topology_until > now;
            let proposal = rapid
                && port.settings.point_to_point
                && !port.edge
                && port.role == Role::Designated
                && port.state != State::Forwarding;
            let priority = Priority {
                bridge: id,
                port: port.settings.id,
                ..self.root_priority
            };
            let bpdu = if !rapid && port.topology_pending && port.role == Role::Root {
                Bpdu::TopologyChange
            } else if port.role == Role::Designated
                || rapid && (port.agreement.is_some() || port.role == Role::Root && topology)
            {
                let mut flags = if topology { wire::TOPOLOGY_CHANGE } else { 0 };
                if rapid {
                    flags |= port.role.flags();
                    if proposal {
                        flags |= wire::PROPOSAL;
                    }
                    if port.agreement.is_some() {
                        flags |= wire::AGREEMENT;
                    }
                    if port.state != State::Discarding {
                        flags |= wire::LEARNING;
                    }
                    if port.state == State::Forwarding {
                        flags |= wire::FORWARDING;
                    }
                } else if port.topology_ack {
                    flags |= wire::TOPOLOGY_ACK;
                }
                Bpdu::Configuration(Configuration {
                    priority,
                    times: self.times,
                    flags,
                    rapid,
                })
            } else {
                continue;
            };
            if port.hold_until <= now {
                port.sent = 0;
                port.hold_until = now + duration(self.times.hello);
            }
            if port.sent >= TX_HOLD {
                port.transmit = port.hold_until;
                continue;
            }
            let mut bytes = [0; 39];
            let payload = &mut bytes[..bpdu.encoded_len()];
            bpdu.emit(payload)
                .expect("selected root times remain valid");
            *budget -= 1;
            let sent = interface.device.send_bridged(
                EthernetHeader {
                    destination: wire::DESTINATION,
                    source: interface.device.mac_address(),
                    type_or_length: payload.len() as u16,
                    tags: &[],
                },
                payload,
            );
            if sent.is_ok() {
                port.sent += 1;
                port.topology_ack = false;
                port.proposal = proposal.then_some(priority);
            }
            port.transmit = now
                + if sent.is_ok() {
                    duration(self.times.hello)
                } else {
                    RETRY
                };
        }
    }
}
