//! 接口的 IGMP 查询器版本、组引用与 codec；ASM 报告/背压状态复用两族的 Listener。
use super::listener::{Listener, Mode, Report};
use crate::inet::SocketError;
use alloc::vec::Vec;
use core::net::{IpAddr, Ipv4Addr};
use packet_wire::{
    igmp::{IgmpMessage, IgmpPacket},
    ipv4::Ipv4Packet,
    multicast::decode_code8,
};

pub(in crate::inet) const ALL_SYSTEMS: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 1);
const ALL_ROUTERS: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 2);
const V3_ROUTERS: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 22);

/// 接口唯一的 IPv4 组播状态；不复制接口地址、载波或配置，socket 只持有成对的组引用。
pub(in crate::inet) struct Groups {
    groups: Vec<Listener<Ipv4Addr>>,
    // OWNER: RFC 9776 §7 的两个接口级旧查询器寿命；少一个会错误跳过仍在工作的旧代。
    v1_until: Option<u64>,
    v2_until: Option<u64>,
    robustness: u8,
    query_interval: u64,
    // OWNER: 成功发送后轮转，持续查询不能让后面的组永久饿死。
    cursor: usize,
}
impl Groups {
    pub(in crate::inet) fn new() -> Self {
        Self {
            groups: Vec::new(),
            v1_until: None,
            v2_until: None,
            robustness: 2,
            query_interval: 125_000,
            cursor: 0,
        }
    }
    pub(in crate::inet) fn accepts(&self, group: Ipv4Addr) -> bool {
        group == ALL_SYSTEMS
            || self
                .groups
                .binary_search_by_key(&group, Listener::group)
                .is_ok_and(|index| self.groups[index].listening())
    }
    /// 先准备唯一监听记录再增加引用；失败不改变当前监听或报告义务。
    pub(in crate::inet) fn add(&mut self, group: Ipv4Addr, now: u64) -> Result<(), SocketError> {
        match self.groups.binary_search_by_key(&group, Listener::group) {
            Ok(index) => {
                let listener = &mut self.groups[index];
                listener.users = listener.users.checked_add(1).ok_or(SocketError::NoMemory)?;
                listener.synchronize(true, now, now, self.robustness);
            }
            Err(index) => {
                self.groups
                    .try_reserve(1)
                    .map_err(|_| SocketError::NoMemory)?;
                let mut listener = Listener::new(group);
                listener.users = 1;
                listener.synchronize(true, now, now, self.robustness);
                self.groups.insert(index, listener);
            }
        }
        Ok(())
    }
    pub(in crate::inet) fn drop_group(&mut self, group: Ipv4Addr, now: u64) {
        if let Ok(index) = self.groups.binary_search_by_key(&group, Listener::group) {
            let listener = &mut self.groups[index];
            listener.users = listener
                .users
                .checked_sub(1)
                .expect("socket owns one IGMP reference");
            listener.synchronize(listener.users != 0, now, now, self.robustness);
            if listener.users == 0 && listener.inactive() {
                self.groups.remove(index);
            }
            if self.groups.is_empty() {
                self.groups = Vec::new();
            }
        }
    }
    /// 撤销协议资格，保留应用引用；恢复接口后重新报告，不复制旧时钟。
    pub(in crate::inet) fn disable(&mut self) {
        for listener in &mut self.groups {
            listener.reset();
        }
        self.groups.retain(|listener| listener.users != 0);
        if self.groups.is_empty() {
            self.groups = Vec::new();
        }
        self.v1_until = None;
        self.v2_until = None;
        self.robustness = 2;
        self.query_interval = 125_000;
    }
    pub(in crate::inet) fn address_ready(&mut self, now: u64) {
        for listener in &mut self.groups {
            if listener.listening() {
                listener.address_ready(now);
            }
        }
    }
    pub(in crate::inet) fn due(&self, now: u64) -> bool {
        [self.v1_until, self.v2_until]
            .into_iter()
            .flatten()
            .any(|at| at <= now)
            || self.groups.iter().any(|listener| listener.due(now))
    }
    fn version(&self, now: u64) -> u8 {
        if self.v1_until.is_some_and(|at| at > now) {
            1
        } else if self.v2_until.is_some_and(|at| at > now) {
            2
        } else {
            3
        }
    }
    /// IP 已完整验证，IGMP 独立检查 checksum/TTL/RA；异常输入不改变计时器或成员。
    pub(in crate::inet) fn receive(&mut self, ip: Ipv4Packet<'_>, now: u64) {
        if ip.protocol() != 2 || ip.fragmented() || ip.hop_limit() != 1 {
            return;
        }
        let Ok(packet) = IgmpPacket::parse(ip.payload()) else {
            return;
        };
        match packet.message() {
            IgmpMessage::Query {
                version,
                maximum_response_code,
                group,
                source_query,
            } => {
                if !group.is_unspecified() && !group.is_multicast()
                    || version == 1 && !group.is_unspecified()
                    || version != 1 && !router_alert(ip.options())
                    || group.is_unspecified()
                        && ip.destination().is_multicast()
                        && ip.destination() != ALL_SYSTEMS
                    || group.is_unspecified()
                        && source_query.is_some_and(|query| query.sources.len() != 0)
                    || source_query.is_some_and(|mut query| {
                        query.sources.any(|source| {
                            source.is_unspecified()
                                || source.is_multicast()
                                || matches!(source, IpAddr::V4(address) if address.is_broadcast())
                        })
                    })
                {
                    return;
                }
                let maximum = match version {
                    1 => 10_000,
                    2 => u64::from(maximum_response_code) * 100,
                    _ => u64::from(decode_code8(maximum_response_code)) * 100,
                };
                if let Some(query) = source_query {
                    self.robustness = if query.robustness == 0 {
                        2
                    } else {
                        query.robustness
                    };
                    self.query_interval = if query.query_interval_code == 0 {
                        125_000
                    } else {
                        u64::from(decode_code8(query.query_interval_code)) * 1000
                    };
                }
                // RFC 9776 §7/8.12：v1 任意合法查询、v2 仅 General Query 刷新对应寿命。
                let expiry = now.saturating_add(
                    u64::from(self.robustness) * self.query_interval + 10 * maximum,
                );
                if version == 1 {
                    self.v1_until = Some(expiry);
                } else if version == 2 && group.is_unspecified() {
                    self.v2_until = Some(expiry);
                }
                for listener in &mut self.groups {
                    listener.query(
                        (!group.is_unspecified()).then_some(group),
                        maximum,
                        source_query.map(|query| query.sources),
                        now,
                    );
                }
            }
            IgmpMessage::Report { group, .. } => {
                // 源 0.0.0.0 的旧代报告同样有效；不把地址前缀当作同链路证明或认证。
                if !group.is_multicast() || ip.destination() != group {
                    return;
                }
                let legacy = self.version(now) != 3;
                if let Ok(index) = self.groups.binary_search_by_key(&group, Listener::group) {
                    self.groups[index].report(group, legacy);
                }
            }
            _ => {}
        }
    }
    /// 每接口每轮最多四次控制提交；预算续调度与设备背压分开，回环不生成 IGMP。
    pub(in crate::inet) fn poll(
        &mut self,
        enabled: bool,
        quiet: bool,
        now: u64,
        mtu: usize,
        mut send: impl FnMut(Report<'_, Ipv4Addr>, u8) -> bool,
    ) -> bool {
        if !enabled {
            self.disable();
            return false;
        }
        let previous = if self.v1_until.is_some() {
            1
        } else if self.v2_until.is_some() {
            2
        } else {
            3
        };
        if self.v1_until.is_some_and(|at| at <= now) {
            self.v1_until = None;
        }
        if self.v2_until.is_some_and(|at| at <= now) {
            self.v2_until = None;
        }
        let version = self.version(now);
        if version != previous {
            self.address_ready(now);
        }
        let mode = if version == 3 {
            Mode::Current
        } else {
            Mode::Legacy {
                leave: version == 2,
            }
        };
        let count = self.groups.len();
        if count == 0 {
            return false;
        }
        let mut remaining = 4u8;
        let mut sent = 0u8;
        let mut blocked = false;
        let mut deferred = false;
        let start = self.cursor % count;
        for step in 0..count {
            let index = (start + step) % count;
            let listener = &mut self.groups[index];
            listener.synchronize(listener.users != 0, now, now, self.robustness);
            if quiet || listener.group() == ALL_SYSTEMS {
                listener.quiet();
                continue;
            }
            let before = sent;
            listener.poll(now, mtu.saturating_sub(40) / 4, mode, &mut |report| {
                if blocked {
                    return false;
                }
                if remaining == 0 {
                    deferred = true;
                    return false;
                }
                remaining -= 1;
                if send(report, version) {
                    sent += 1;
                    true
                } else {
                    blocked = true;
                    false
                }
            });
            if before != sent {
                self.cursor = (index + 1) % count;
            }
        }
        self.groups
            .retain(|listener| listener.users != 0 || !listener.inactive());
        if self.groups.is_empty() {
            self.groups = Vec::new();
        }
        deferred && !blocked
    }
}
fn router_alert(mut options: &[u8]) -> bool {
    while let Some(&kind) = options.first() {
        if kind == 0 {
            return false;
        }
        if kind == 1 {
            options = &options[1..];
            continue;
        }
        if options.len() < 2 {
            return false;
        }
        let length = usize::from(options[1]);
        if length < 2 || length > options.len() {
            return false;
        }
        if kind == 0x94 && length == 4 && options[2..4] == [0, 0] {
            return true;
        }
        options = &options[length..];
    }
    false
}

/// 在最终 NIC 窗口直接生成报告；仅借用来源，无临时帧和第二份组记录。
pub(in crate::inet) fn send(
    device: &crate::inet::device::ProtocolDevice,
    source: Ipv4Addr,
    report: Report<'_, Ipv4Addr>,
    version: u8,
    timestamp: smoltcp::time::Instant,
) -> bool {
    let (destination, length) = match &report {
        Report::Legacy { group, leave } => (if *leave { ALL_ROUTERS } else { *group }, 32),
        Report::Current { sources, .. } => (V3_ROUTERS, 40 + sources.len() * 4),
    };
    if length > device.ip_mtu() {
        device.record_transmit_drop();
        return false;
    }
    let Some(token) = device.transmit_shared(timestamp) else {
        return false;
    };
    let (_, failure) = token.consume_committed(length + 14, 0x0800, |frame| {
        let address = destination.octets();
        packet_wire::ethernet::EthernetHeader {
            source: device.mac_address(),
            destination: [1, 0, 0x5e, address[1] & 0x7f, address[2], address[3]],
            type_or_length: 0x0800,
            tags: &[],
        }
        .emit(frame)
        .expect("exact IGMP Ethernet window");
        let packet = &mut frame[14..];
        match report {
            Report::Legacy { group, leave } => packet_wire::igmp::emit_report(
                &mut packet[24..],
                if leave {
                    0x17
                } else if version == 1 {
                    0x12
                } else {
                    0x16
                },
                group,
            )
            .expect("exact legacy IGMP report"),
            Report::Current {
                group,
                kind,
                sources,
            } => {
                let record = &mut packet[32..];
                packet_wire::multicast::GroupRecordHeader {
                    record_type: kind,
                    group: group.into(),
                    source_count: sources.len() as u16,
                }
                .emit(record)
                .expect("bounded IGMP group record");
                for (bytes, source) in record[8..].as_chunks_mut::<4>().0.iter_mut().zip(sources) {
                    bytes.copy_from_slice(&source.octets());
                }
                packet_wire::igmp::emit_report_v3(&mut packet[24..], 1, 0)
                    .expect("exact IGMPv3 report");
            }
        }
        packet_wire::ipv4::Ipv4Header {
            source,
            destination,
            protocol: 2,
            hop_limit: 1,
            dscp_ecn: 0xc0,
            identification: 0,
            fragment: 0x4000,
            options: &[0x94, 4, 0, 0],
        }
        .emit(packet)
        .expect("admitted IGMP size");
    });
    if failure.is_some() {
        device.record_transmit_drop();
    }
    failure.is_none()
}
