//! NETLINK_ROUTE 的纯消息 codec：nlmsghdr、ifinfomsg/ifaddrmsg/rtmsg 与属性编码，以及请求解析。
//!
//! 不读取内核状态；调用者提供冻结快照与已预留的输出。固定头使用栈缓冲，变长路径组直接编码到
//! 输出；路径布局在建立 dump 时验证，接收期间不分配，也不推进尚未准备好存储的消息。

use crate::net::{InterfaceSnapshot, Ipv4RouteKind, RoutingSnapshot};
use alloc::vec::Vec;
use device_model::network::NetworkStatistics;
use network_protocol::RouteSnapshot;

pub(super) const NLMSG_ERROR: u16 = 2;
pub(super) const NLMSG_DONE: u16 = 3;
pub(super) const NLMSG_MIN_TYPE: u16 = 0x10;
pub(super) const RTM_NEWLINK: u16 = 16;
pub(super) const RTM_DELLINK: u16 = 17;
pub(super) const RTM_GETLINK: u16 = 18;
pub(super) const RTM_SETLINK: u16 = 19;
pub(super) const RTM_NEWADDR: u16 = 20;
pub(super) const RTM_DELADDR: u16 = 21;
pub(super) const RTM_GETADDR: u16 = 22;
pub(super) const RTM_NEWROUTE: u16 = 24;
pub(super) const RTM_DELROUTE: u16 = 25;
pub(super) const RTM_GETROUTE: u16 = 26;
/// 最大合法 rtnetlink 消息类型（RTM_GETTFILTER 之后）。
pub(super) const RTM_MAX: u16 = 0x6f;

pub(super) const NLM_F_REQUEST: u16 = 0x1;
pub(super) const NLM_F_MULTI: u16 = 0x2;
pub(super) const NLM_F_ACK: u16 = 0x4;
pub(super) const NLM_F_REPLACE: u16 = 0x100;
pub(super) const NLM_F_EXCL: u16 = 0x200;
pub(super) const NLM_F_CREATE: u16 = 0x400;
pub(super) const NLM_F_APPEND: u16 = 0x800;
/// `NLM_F_ROOT | NLM_F_MATCH`：请求整张表。
pub(super) const NLM_F_DUMP: u16 = 0x300;

pub(super) const HEADER_LENGTH: usize = 16;
const AF_INET: u8 = 2;
const ARPHRD_ETHER: u16 = 1;
const ARPHRD_LOOPBACK: u16 = 772;
const IFF_LOWER_UP: u32 = 0x10000;

const IFLA_ADDRESS: u16 = 1;
const IFLA_BROADCAST: u16 = 2;
const IFLA_IFNAME: u16 = 3;
const IFLA_MTU: u16 = 4;
const IFLA_QDISC: u16 = 6;
const IFLA_TXQLEN: u16 = 13;
const IFLA_OPERSTATE: u16 = 16;
const IFLA_LINKMODE: u16 = 17;
const IFLA_STATS64: u16 = 23;
const IFLA_GROUP: u16 = 27;
const IFLA_PROMISCUITY: u16 = 30;
const IFLA_NUM_TX_QUEUES: u16 = 31;
const IFLA_NUM_RX_QUEUES: u16 = 32;
const IFLA_CARRIER: u16 = 33;
const IFLA_MIN_MTU: u16 = 50;

const IFA_ADDRESS: u16 = 1;
const IFA_LOCAL: u16 = 2;
const IFA_LABEL: u16 = 3;
const IFA_BROADCAST: u16 = 4;
const IFA_CACHEINFO: u16 = 6;
const IFA_FLAGS: u16 = 8;
const IFA_F_PERMANENT: u32 = 0x80;
const RT_SCOPE_HOST: u8 = 254;

const RTA_DST: u16 = 1;
const RTA_OIF: u16 = 4;
const RTA_GATEWAY: u16 = 5;
const RTA_PRIORITY: u16 = 6;
const RTA_PREFSRC: u16 = 7;
const RTA_METRICS: u16 = 8;
const RTA_MULTIPATH: u16 = 9;
const RTA_TABLE: u16 = 15;
const RTAX_WINDOW: u16 = 3;
const RTAX_RTT: u16 = 4;
const RTAX_ADVMSS: u16 = 8;
const RTNH_F_ONLINK: u32 = 4;
const RT_TABLE_COMPAT: u8 = 252;

/// 单条消息的栈缓冲上限；最大的 link 消息约 360 字节。
const MESSAGE_CAPACITY: usize = 1024;
const ENOMEM: i32 = 12;
const EOVERFLOW: i32 = 75;
const EMSGSIZE: i32 = 90;

fn align4(value: usize) -> usize {
    (value + 3) & !3
}

/// 一条正在构造的 netlink 消息。
pub(super) struct Message {
    bytes: [u8; MESSAGE_CAPACITY],
    length: usize,
}

impl Message {
    /// 开始一条消息；`nlmsg_len` 在 [`Self::finish`] 回填。
    pub(super) fn new(kind: u16, flags: u16, sequence: u32, port: u32) -> Self {
        let mut message = Self {
            bytes: [0; MESSAGE_CAPACITY],
            length: HEADER_LENGTH,
        };
        message.bytes[4..6].copy_from_slice(&kind.to_ne_bytes());
        message.bytes[6..8].copy_from_slice(&flags.to_ne_bytes());
        message.bytes[8..12].copy_from_slice(&sequence.to_ne_bytes());
        message.bytes[12..16].copy_from_slice(&port.to_ne_bytes());
        message
    }

    fn put(&mut self, data: &[u8]) {
        let end = self.length + data.len();
        assert!(end <= MESSAGE_CAPACITY, "rtnetlink message exceeds scratch");
        self.bytes[self.length..end].copy_from_slice(data);
        self.length = align4(end);
    }

    fn attribute(&mut self, kind: u16, payload: &[u8]) {
        // nlattr：u16 长度（含 4 字节头，不含尾部 padding）、u16 类型，随后是对齐到 4 的负载。
        self.put_raw(&((4 + payload.len()) as u16).to_ne_bytes());
        self.put_raw(&kind.to_ne_bytes());
        self.put(payload);
    }

    /// 写入但不做对齐推进。
    fn put_raw(&mut self, data: &[u8]) {
        let end = self.length + data.len();
        assert!(end <= MESSAGE_CAPACITY, "rtnetlink message exceeds scratch");
        self.bytes[self.length..end].copy_from_slice(data);
        self.length = end;
    }

    fn attribute_u32(&mut self, kind: u16, value: u32) {
        self.attribute(kind, &value.to_ne_bytes());
    }

    fn attribute_u8(&mut self, kind: u16, value: u8) {
        self.attribute(kind, &[value]);
    }

    fn attribute_string(&mut self, kind: u16, text: &[u8]) {
        let mut padded = [0u8; 17];
        padded[..text.len()].copy_from_slice(text);
        self.attribute(kind, &padded[..text.len() + 1]);
    }

    /// 回填长度并返回完整消息字节。
    pub(super) fn finish(mut self) -> ([u8; MESSAGE_CAPACITY], usize) {
        let length = self.length as u32;
        self.bytes[..4].copy_from_slice(&length.to_ne_bytes());
        (self.bytes, self.length)
    }
}

fn interface_name(snapshot: &InterfaceSnapshot) -> &[u8] {
    let end = snapshot
        .name
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or_else(|| snapshot.name.len().min(15));
    &snapshot.name[..end.min(15)]
}

/// `RTM_NEWLINK`：一个网络设备。
pub(super) fn link(
    snapshot: &InterfaceSnapshot,
    statistics: &NetworkStatistics,
    flags: u16,
    sequence: u32,
    port: u32,
) -> ([u8; MESSAGE_CAPACITY], usize) {
    let mut message = Message::new(RTM_NEWLINK, flags, sequence, port);
    let hardware_type = if snapshot.loopback {
        ARPHRD_LOOPBACK
    } else {
        ARPHRD_ETHER
    };
    let carrier = snapshot.carrier() == Some(true);
    let interface_flags = u32::from(snapshot.flags())
        | if snapshot.up && carrier {
            IFF_LOWER_UP
        } else {
            0
        };
    // ifinfomsg（16 字节）：family、pad、type(u16)、index(i32)、flags、change。必须一次写完整，
    // 否则 `put` 对 2 字节前缀做的对齐会把 type 推到错误偏移。
    let mut info = [0u8; 16];
    info[2..4].copy_from_slice(&hardware_type.to_ne_bytes());
    info[4..8].copy_from_slice(&(snapshot.index as i32).to_ne_bytes());
    info[8..12].copy_from_slice(&interface_flags.to_ne_bytes());
    message.put(&info);
    let (address, broadcast) = if snapshot.loopback {
        ([0u8; 6], [0u8; 6])
    } else {
        (snapshot.mac, [0xff; 6])
    };
    message.attribute(IFLA_ADDRESS, &address);
    message.attribute(IFLA_BROADCAST, &broadcast);
    message.attribute_string(IFLA_IFNAME, interface_name(snapshot));
    message.attribute_u32(IFLA_MTU, snapshot.mtu as u32);
    message.attribute_u32(IFLA_MIN_MTU, 68);
    message.attribute_string(IFLA_QDISC, b"noqueue");
    message.attribute_u32(IFLA_TXQLEN, 1000);
    message.attribute_u8(IFLA_OPERSTATE, snapshot.operstate());
    message.attribute_u8(IFLA_LINKMODE, 0);
    message.attribute_u32(IFLA_GROUP, 0);
    message.attribute_u32(IFLA_PROMISCUITY, 0);
    message.attribute_u32(IFLA_NUM_TX_QUEUES, 1);
    message.attribute_u32(IFLA_NUM_RX_QUEUES, 1);
    message.attribute_u8(IFLA_CARRIER, u8::from(carrier));
    // rtnl_link_stats64 的 24 个 u64，未维护的计数为 0。
    let mut counters = [0u64; 24];
    counters[0] = statistics.received_packets;
    counters[1] = statistics.transmitted_packets;
    counters[2] = statistics.received_bytes;
    counters[3] = statistics.transmitted_bytes;
    counters[4] = statistics.received_errors;
    counters[5] = statistics.transmitted_errors;
    counters[6] = statistics.received_dropped;
    counters[7] = statistics.transmitted_dropped;
    let mut encoded = [0u8; 192];
    for (chunk, counter) in encoded.as_chunks_mut::<8>().0.iter_mut().zip(counters) {
        chunk.copy_from_slice(&counter.to_ne_bytes());
    }
    message.attribute(IFLA_STATS64, &encoded);
    message.finish()
}

/// `RTM_NEWADDR`：设备的 IPv4 地址；设备没有地址时返回 `None`。
pub(super) fn address(
    snapshot: &InterfaceSnapshot,
    sequence: u32,
    port: u32,
) -> Option<([u8; MESSAGE_CAPACITY], usize)> {
    let local = snapshot.address?;
    let mut message = Message::new(RTM_NEWADDR, NLM_F_MULTI, sequence, port);
    let scope = if snapshot.loopback { RT_SCOPE_HOST } else { 0 };
    // ifaddrmsg：family、prefixlen、flags、scope、index。
    message.put(&[
        AF_INET,
        snapshot.prefix_length,
        IFA_F_PERMANENT as u8,
        scope,
    ]);
    message.put(&snapshot.index.to_ne_bytes());
    message.attribute(IFA_ADDRESS, &local.octets());
    message.attribute(IFA_LOCAL, &local.octets());
    message.attribute_string(IFA_LABEL, interface_name(snapshot));
    if !snapshot.loopback {
        message.attribute(IFA_BROADCAST, &snapshot.broadcast.octets());
    }
    // 永久地址：prefered/valid 生存期为无穷（0xffffffff），时间戳 0。
    let mut cache = [0u8; 16];
    cache[..4].copy_from_slice(&u32::MAX.to_ne_bytes());
    cache[4..8].copy_from_slice(&u32::MAX.to_ne_bytes());
    message.attribute(IFA_CACHEINFO, &cache);
    message.attribute_u32(IFA_FLAGS, IFA_F_PERMANENT);
    Some(message.finish())
}

fn route_type(kind: Ipv4RouteKind) -> u8 {
    match kind {
        Ipv4RouteKind::Unicast => 1,
        Ipv4RouteKind::Local => 2,
        Ipv4RouteKind::Broadcast => 3,
        Ipv4RouteKind::Blackhole => 6,
        Ipv4RouteKind::Prohibit => 8,
        Ipv4RouteKind::Unreachable => 7,
        Ipv4RouteKind::Throw => 9,
    }
}

/// 冻结组的 Linux 表示：只保存长度与精确权重公因数，不复制成员或建立逐路由对象。
struct GroupEncoding {
    attribute_length: u16,
    weight_divisor: u32,
}

/// Linux dump 的唯一捕获正文和每组表示；布局只由不可变正文推导，发布后无需人工同步。
pub(super) struct RouteDump {
    snapshot: RoutingSnapshot,
    groups: Vec<GroupEncoding>,
    maximum_message_size: usize,
}
impl RouteDump {
    /// native 权重为 u32；Linux rtnh_hops 仅有八位。先约分，不能精确表示则 EOVERFLOW；
    /// 完整 RTA_MULTIPATH 超出 u16 长度则 EMSGSIZE，均不截断或修改执行中的 FIB。
    pub(super) fn new(snapshot: RoutingSnapshot) -> Result<Self, i32> {
        let mut groups = Vec::new();
        groups
            .try_reserve_exact(snapshot.group_count())
            .map_err(|_| -ENOMEM)?;
        let mut maximum_message_size = MESSAGE_CAPACITY;
        for index in 0..snapshot.group_count() {
            let group = snapshot.group(index).expect("bounded snapshot group");
            let mut length = 4usize;
            let mut divisor = 0;
            for member in group.members() {
                i32::try_from(member.interface_index).map_err(|_| -EOVERFLOW)?;
                length = length
                    .checked_add(if member.gateway.is_some() { 16 } else { 8 })
                    .ok_or(-EMSGSIZE)?;
                divisor = gcd(divisor, member.weight);
            }
            let attribute_length = if group.len() > 1 {
                let length = u16::try_from(length).map_err(|_| -EMSGSIZE)?;
                if group.members().any(|member| member.weight / divisor > 256) {
                    return Err(-EOVERFLOW);
                }
                // 104 是所有固定 route 属性同时出现时的保守上界；实际消息长度按需编码。
                maximum_message_size = maximum_message_size.max(104 + usize::from(length));
                length
            } else {
                0
            };
            groups.push(GroupEncoding {
                attribute_length,
                weight_divisor: divisor,
            });
        }
        Ok(Self {
            snapshot,
            groups,
            maximum_message_size,
        })
    }
    pub(super) fn len(&self) -> usize {
        self.snapshot.len()
    }
    pub(super) fn maximum_message_size(&self) -> usize {
        self.maximum_message_size
    }

    /// 有足够容量才追加完整消息；首条允许超过用户缓冲，由 recvmsg 的截断语义处理。
    pub(super) fn append(
        &self,
        index: usize,
        sequence: u32,
        port: u32,
        limit: usize,
        out: &mut Vec<u8>,
    ) -> bool {
        let row = self.snapshot.row(index).expect("bounded dump row");
        let (bytes, header_length) = route_header(row, sequence, port);
        let layout = row
            .next_hops
            .map(|group| &self.groups[group.id as usize - 1]);
        let length =
            header_length + layout.map_or(0, |layout| usize::from(layout.attribute_length));
        if !out.is_empty() && out.len() + length > limit {
            return false;
        }
        // receive 在取 state guard 前已预留 max(window, maximum_message_size)，以下不得分配。
        assert!(
            out.capacity() - out.len() >= length,
            "route dump append without prepared capacity"
        );
        let start = out.len();
        out.extend_from_slice(&bytes[..header_length]);
        if let Some(layout) = layout.filter(|layout| layout.attribute_length != 0) {
            out.extend_from_slice(&layout.attribute_length.to_ne_bytes());
            out.extend_from_slice(&RTA_MULTIPATH.to_ne_bytes());
            for member in row.next_hops.expect("multipath group").members() {
                let length: u16 = if member.gateway.is_some() { 16 } else { 8 };
                out.extend_from_slice(&length.to_ne_bytes());
                out.push(if member.on_link {
                    RTNH_F_ONLINK as u8
                } else {
                    0
                });
                out.push((member.weight / layout.weight_divisor - 1) as u8);
                out.extend_from_slice(&(member.interface_index as i32).to_ne_bytes());
                if let Some(gateway) = member.gateway {
                    out.extend_from_slice(&8u16.to_ne_bytes());
                    out.extend_from_slice(&RTA_GATEWAY.to_ne_bytes());
                    out.extend_from_slice(&gateway.octets());
                }
            }
        }
        out[start..start + 4].copy_from_slice(&(length as u32).to_ne_bytes());
        true
    }
}
fn gcd(mut a: u32, mut b: u32) -> u32 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

/// `RTM_NEWROUTE` 的固定头与标量属性；多路径属性由 RouteDump 直接追加。
fn route_header(
    row: RouteSnapshot<'_>,
    sequence: u32,
    port: u32,
) -> ([u8; MESSAGE_CAPACITY], usize) {
    let route = row.route;
    let single = row.single_path();
    let mut message = Message::new(RTM_NEWROUTE, NLM_F_MULTI, sequence, port);
    let compat_table = u8::try_from(route.table).unwrap_or(RT_TABLE_COMPAT);
    let flags = if single.is_some_and(|route| route.on_link) {
        RTNH_F_ONLINK
    } else {
        0
    };
    // rtmsg：family、dst_len、src_len、tos、table、protocol、scope、type、flags。
    message.put(&[
        AF_INET,
        route.prefix_length,
        0,
        0,
        compat_table,
        route.protocol,
        route.scope,
        route_type(route.kind),
    ]);
    message.put(&flags.to_ne_bytes());
    message.attribute_u32(RTA_TABLE, route.table);
    if route.prefix_length > 0 {
        message.attribute(RTA_DST, &route.destination.octets());
    }
    if let Some(index) = single.and_then(|route| route.interface_index) {
        message.attribute_u32(RTA_OIF, index);
    }
    if let Some(gateway) = single.and_then(|route| route.gateway) {
        message.attribute(RTA_GATEWAY, &gateway.octets());
    }
    if let Some(source) = route.preferred_source {
        message.attribute(RTA_PREFSRC, &source.octets());
    }
    if route.metric != 0 {
        message.attribute_u32(RTA_PRIORITY, route.metric);
    }
    let metrics = route.metrics;
    if metrics.advertised_mss != 0 || metrics.receive_window != 0 || metrics.round_trip_time != 0 {
        // 嵌套属性：RTAX_* 标量，各自是 u32。
        let mut nested = [0u8; 36];
        let mut length = 0;
        for (kind, value) in [
            (RTAX_WINDOW, metrics.receive_window),
            (RTAX_RTT, metrics.round_trip_time),
            (RTAX_ADVMSS, metrics.advertised_mss),
        ] {
            if value == 0 {
                continue;
            }
            nested[length..length + 2].copy_from_slice(&8u16.to_ne_bytes());
            nested[length + 2..length + 4].copy_from_slice(&kind.to_ne_bytes());
            nested[length + 4..length + 8].copy_from_slice(&value.to_ne_bytes());
            length += 8;
        }
        message.attribute(RTA_METRICS, &nested[..length]);
    }
    message.finish()
}

/// `NLMSG_ERROR`：`error` 为 0 表示 ACK；非零是负 errno，并带回完整原始请求。
pub(super) fn error(
    error: i32,
    request: &[u8],
    sequence: u32,
    port: u32,
    out: &mut alloc::vec::Vec<u8>,
) -> Result<(), ()> {
    let echoed = if error == 0 {
        &request[..HEADER_LENGTH.min(request.len())]
    } else {
        request
    };
    let length = HEADER_LENGTH + 4 + echoed.len();
    out.try_reserve_exact(align4(length)).map_err(|_| ())?;
    out.extend_from_slice(&(length as u32).to_ne_bytes());
    out.extend_from_slice(&NLMSG_ERROR.to_ne_bytes());
    out.extend_from_slice(&0u16.to_ne_bytes());
    out.extend_from_slice(&sequence.to_ne_bytes());
    out.extend_from_slice(&port.to_ne_bytes());
    out.extend_from_slice(&error.to_ne_bytes());
    out.extend_from_slice(echoed);
    out.resize(align4(length), 0);
    Ok(())
}

/// `NLMSG_DONE`：dump 结束，负载是 0。
pub(super) fn done(sequence: u32, port: u32) -> ([u8; MESSAGE_CAPACITY], usize) {
    let mut message = Message::new(NLMSG_DONE, NLM_F_MULTI, sequence, port);
    message.put(&0i32.to_ne_bytes());
    message.finish()
}

/// 已校验的请求头。
#[derive(Clone, Copy)]
pub(super) struct RequestHeader {
    pub(super) length: usize,
    pub(super) kind: u16,
    pub(super) flags: u16,
    pub(super) sequence: u32,
}

/// 从 `bytes` 开头解析 nlmsghdr；长度不足或声明长度越界返回 `None`（Linux 同样停止处理余下部分）。
pub(super) fn parse_header(bytes: &[u8]) -> Option<RequestHeader> {
    if bytes.len() < HEADER_LENGTH {
        return None;
    }
    let length = u32::from_ne_bytes(bytes[..4].try_into().ok()?) as usize;
    if length < HEADER_LENGTH || length > bytes.len() {
        return None;
    }
    Some(RequestHeader {
        length,
        kind: u16::from_ne_bytes(bytes[4..6].try_into().ok()?),
        flags: u16::from_ne_bytes(bytes[6..8].try_into().ok()?),
        sequence: u32::from_ne_bytes(bytes[8..12].try_into().ok()?),
    })
}

/// 下一条消息在数据报中的起点（`NLMSG_ALIGN`）。
pub(super) fn next_offset(length: usize) -> usize {
    align4(length)
}

/// 遍历 `payload` 的属性区；`skip` 是固定头长度（对齐后跳过）。属性头越界时迭代提前结束。
///
/// 每项是 `(属性类型, 负载)`；类型已去掉 `NLA_F_NESTED`/`NLA_F_NET_BYTEORDER` 位。
pub(super) fn attributes(payload: &[u8], skip: usize) -> impl Iterator<Item = (u16, &[u8])> {
    let mut offset = align4(skip);
    core::iter::from_fn(move || {
        if offset + 4 > payload.len() {
            return None;
        }
        let length = usize::from(u16::from_ne_bytes([payload[offset], payload[offset + 1]]));
        let kind = u16::from_ne_bytes([payload[offset + 2], payload[offset + 3]]) & 0x3fff;
        if length < 4 || offset + length > payload.len() {
            return None;
        }
        let data = &payload[offset + 4..offset + length];
        offset += align4(length);
        Some((kind, data))
    })
}

/// 找 `kind` 属性的负载。
pub(super) fn find_attribute(payload: &[u8], skip: usize, kind: u16) -> Option<&[u8]> {
    attributes(payload, skip).find_map(|(found, data)| (found == kind).then_some(data))
}

/// 请求里用于按名字查询设备的属性类型。
pub(super) const IFLA_IFNAME_ATTRIBUTE: u16 = IFLA_IFNAME;
