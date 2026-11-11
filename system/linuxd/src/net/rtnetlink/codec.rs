//! NETLINK_ROUTE 的纯消息 codec：nlmsghdr、ifinfomsg/ifaddrmsg/rtmsg/ndmsg 与属性编码，以及请求解析。
//!
//! 不读取内核状态；调用者提供冻结快照与已预留的输出。固定头使用栈缓冲，变长路径组直接编码到
//! 输出；路径布局在建立 dump 时验证，接收期间不分配，也不推进尚未准备好存储的消息。

use crate::net::{InterfaceSnapshot, RouteKind, RoutingSnapshot};
use alloc::vec::Vec;
use core::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use device_model::network::NetworkStatistics;
use network_protocol::routes::{RouteDirection, RouteLookup, RoutePath};

mod rules;
use network_protocol::RouteSnapshot;
pub(super) use rules::{message as rule, query as rule_query};

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
pub(super) const RTM_NEWNEIGH: u16 = 28;
pub(super) const RTM_DELNEIGH: u16 = 29;
pub(super) const RTM_GETNEIGH: u16 = 30;
pub(super) const RTM_NEWRULE: u16 = 32;
pub(super) const RTM_DELRULE: u16 = 33;
pub(super) const RTM_GETRULE: u16 = 34;
pub(super) const NLM_F_DUMP_FILTERED: u16 = 0x20;
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
const AF_INET6: u8 = 10;
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
const RTA_SRC: u16 = 2;
const RTA_IIF: u16 = 3;
const RTA_OIF: u16 = 4;
const RTA_GATEWAY: u16 = 5;
const RTA_PRIORITY: u16 = 6;
const RTA_PREFSRC: u16 = 7;
const RTA_METRICS: u16 = 8;
const RTA_MULTIPATH: u16 = 9;
const RTA_VIA: u16 = 18;
const RTA_TABLE: u16 = 15;
const RTA_MARK: u16 = 16;
const RTAX_WINDOW: u16 = 3;
const RTAX_RTT: u16 = 4;
const RTAX_ADVMSS: u16 = 8;
const RTNH_F_ONLINK: u32 = 4;
const RT_TABLE_COMPAT: u8 = 252;
const RTM_F_LOOKUP_TABLE: u32 = 0x1000;

/// 单条消息的栈缓冲上限；最大的 link 消息约 360 字节。
const MESSAGE_CAPACITY: usize = 1024;
const ENOMEM: i32 = 12;
const EOVERFLOW: i32 = 75;
const EMSGSIZE: i32 = 90;
const EINVAL: i32 = 22;
const EOPNOTSUPP: i32 = 95;

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

/// 冻结的实际地址直接投影为 RTM_NEWADDR；无地址的接口不再生成占位行。
pub(super) fn address(
    row: network_protocol::InterfaceAddress,
    sequence: u32,
    port: u32,
) -> ([u8; MESSAGE_CAPACITY], usize) {
    use network_protocol::AddressState;
    let flags = if row.origin == network_protocol::AddressOrigin::Slaac {
        0
    } else {
        IFA_F_PERMANENT
    } | if row.address.is_ipv6() && !row.on_link {
        0x200
    } else {
        0
    } | match row.state {
        AddressState::Tentative => 0x40,
        AddressState::Preferred => 0,
        AddressState::Duplicate => 0x40 | 0x08,
        AddressState::Deprecated => 0x20,
    };
    let family = if row.address.is_ipv4() {
        AF_INET
    } else {
        AF_INET6
    };
    let scope = if row.address.is_loopback() {
        RT_SCOPE_HOST
    } else if matches!(row.address, core::net::IpAddr::V6(address) if address.is_unicast_link_local())
    {
        253
    } else {
        0
    };
    let mut message = Message::new(RTM_NEWADDR, NLM_F_MULTI, sequence, port);
    message.put(&[family, row.prefix_length, flags as u8, scope]);
    message.put(&row.interface_index.to_ne_bytes());
    match row.address {
        core::net::IpAddr::V4(local) => {
            message.attribute(IFA_ADDRESS, &local.octets());
            message.attribute(IFA_LOCAL, &local.octets());
            let end = row
                .name
                .iter()
                .position(|byte| *byte == 0)
                .unwrap_or(row.name.len());
            message.attribute_string(IFA_LABEL, &row.name[..end]);
            if let Some(broadcast) = row.broadcast {
                message.attribute(IFA_BROADCAST, &broadcast.octets());
            }
        }
        core::net::IpAddr::V6(local) => message.attribute(IFA_ADDRESS, &local.octets()),
    }
    let mut cache = [0; 16];
    cache[..4].copy_from_slice(&row.preferred_lifetime.to_ne_bytes());
    cache[4..8].copy_from_slice(&row.valid_lifetime.to_ne_bytes());
    message.attribute(IFA_CACHEINFO, &cache);
    message.attribute_u32(IFA_FLAGS, flags);
    message.finish()
}
/// Linux ifindex 有符号位宽在应用投影边界验证；原生事实不按应用 ABI 截断。
pub(super) fn validate_addresses(
    rows: &network_protocol::AddressSnapshot,
    family: u8,
) -> Result<(), i32> {
    for index in 0..rows.len() {
        let row = rows.get(index).expect("frozen address row");
        if (family == 0 || (family == AF_INET) == row.address.is_ipv4())
            && row.interface_index > i32::MAX as u32
        {
            return Err(75);
        }
    }
    Ok(())
}

fn route_type(kind: RouteKind) -> u8 {
    match kind {
        RouteKind::Unicast => 1,
        RouteKind::Local => 2,
        RouteKind::Broadcast => 3,
        RouteKind::Blackhole => 6,
        RouteKind::Prohibit => 8,
        RouteKind::Unreachable => 7,
        RouteKind::Throw => 9,
    }
}

/// 解码本机输出或指定入接口的路径点查；尚无执行语义的键返回错误。
pub(super) fn route_query(payload: &[u8]) -> Result<RouteLookup, i32> {
    let head = payload.get(..12).ok_or(EINVAL)?;
    let (length, unspecified) = match head[0] {
        AF_INET => (32, IpAddr::from(Ipv4Addr::UNSPECIFIED)),
        AF_INET6 => (128, IpAddr::from(Ipv6Addr::UNSPECIFIED)),
        _ => return Err(EOPNOTSUPP),
    };
    if head[1] != length || head[2] != 0 && head[2] != length || head[4..8] != [0; 4] {
        return Err(EINVAL);
    }
    let flags = u32::from_ne_bytes(head[8..12].try_into().expect("rtm_flags"));
    let supported_flags = if head[0] == AF_INET {
        RTM_F_LOOKUP_TABLE
    } else {
        0
    };
    if head[3] != 0 || flags & !supported_flags != 0 {
        return Err(EOPNOTSUPP);
    }
    let address = |bytes: &[u8]| {
        if length == 32 {
            let bytes: [u8; 4] = bytes.try_into().map_err(|_| EINVAL)?;
            Ok::<IpAddr, i32>(Ipv4Addr::from(bytes).into())
        } else {
            let bytes: [u8; 16] = bytes.try_into().map_err(|_| EINVAL)?;
            let address = Ipv6Addr::from(bytes);
            if address.to_ipv4_mapped().is_some() {
                return Err(EINVAL);
            }
            Ok(address.into())
        }
    };
    let mut destination = None;
    let mut source = None;
    let mut incoming = None;
    let mut outgoing = None;
    let mut mark = None;
    for (kind, bytes) in validated_attributes(payload, 12)? {
        match kind {
            RTA_DST if destination.is_none() => destination = Some(address(bytes)?),
            RTA_SRC if source.is_none() && head[2] == length => source = Some(address(bytes)?),
            RTA_IIF | RTA_OIF => {
                let interface = if kind == RTA_IIF {
                    &mut incoming
                } else {
                    &mut outgoing
                };
                let index = i32::from_ne_bytes(bytes.try_into().map_err(|_| EINVAL)?);
                if index <= 0 || interface.is_some() {
                    return Err(EINVAL);
                }
                *interface = Some(index as u32);
            }
            RTA_MARK if mark.is_none() => {
                mark = Some(u32::from_ne_bytes(bytes.try_into().map_err(|_| EINVAL)?));
            }
            RTA_DST | RTA_SRC | RTA_MARK => return Err(EINVAL),
            _ => return Err(EOPNOTSUPP),
        }
    }
    if (head[2] != 0) != source.is_some() {
        return Err(EINVAL);
    }
    Ok(RouteLookup {
        destination: destination.ok_or(EINVAL)?,
        source: source.unwrap_or(unspecified),
        mark: mark.unwrap_or(0),
        // 与 Linux 的输入查找一致：指定 iif 时进入输入路径，oif 只约束本机输出。
        direction: incoming.map_or(RouteDirection::Output(outgoing), RouteDirection::Input),
    })
}

/// 输出一条普通 RTM_NEWROUTE 回复，网关可以是跨族 RTA_VIA。
pub(super) fn route_path(
    query: RouteLookup,
    path: RoutePath,
    sequence: u32,
    port: u32,
) -> Result<Vec<u8>, i32> {
    if path.interface_index > i32::MAX as u32 {
        return Err(EOVERFLOW);
    }
    let (bytes, length) = match (query.destination, query.source, path.source) {
        (IpAddr::V4(destination), IpAddr::V4(source), IpAddr::V4(chosen)) => route_path_message(
            destination,
            source,
            chosen,
            (query.direction, query.mark),
            path,
            sequence,
            port,
        ),
        (IpAddr::V6(destination), IpAddr::V6(source), IpAddr::V6(chosen)) => route_path_message(
            destination,
            source,
            chosen,
            (query.direction, query.mark),
            path,
            sequence,
            port,
        ),
        _ => return Err(EINVAL),
    };
    let mut datagram = Vec::new();
    datagram.try_reserve_exact(length).map_err(|_| ENOMEM)?;
    datagram.extend_from_slice(&bytes[..length]);
    Ok(datagram)
}

fn route_path_message<A: RouteAddress>(
    destination: A,
    source: A,
    chosen: A,
    (direction, mark): (RouteDirection, u32),
    path: RoutePath,
    sequence: u32,
    port: u32,
) -> ([u8; MESSAGE_CAPACITY], usize) {
    let length = (A::BYTES * 8) as u8;
    let table = u8::try_from(path.table).unwrap_or(RT_TABLE_COMPAT);
    let mut message = Message::new(RTM_NEWROUTE, 0, sequence, port);
    message.put(&[
        A::LINUX_FAMILY,
        length,
        if source == A::UNSPECIFIED { 0 } else { length },
        0,
        table,
        0,
        if path.kind == RouteKind::Local {
            RT_SCOPE_HOST
        } else {
            0
        },
        route_type(path.kind),
    ]);
    message.put(&0u32.to_ne_bytes());
    message.attribute(RTA_DST, destination.octets().as_ref());
    if source != A::UNSPECIFIED {
        message.attribute(RTA_SRC, source.octets().as_ref());
    }
    message.attribute_u32(RTA_TABLE, path.table);
    message.attribute_u32(RTA_OIF, path.interface_index);
    if mark != 0 {
        message.attribute_u32(RTA_MARK, mark);
    }
    if let RouteDirection::Input(index) = direction {
        message.attribute_u32(RTA_IIF, index);
    }
    if let Some(gateway) = path.gateway.and_then(A::gateway) {
        let (kind, bytes, size) = gateway_attribute::<A>(gateway);
        message.attribute(kind, &bytes[..size]);
    }
    if matches!(direction, RouteDirection::Output(_))
        && chosen != A::UNSPECIFIED
        && source == A::UNSPECIFIED
    {
        message.attribute(RTA_PREFSRC, chosen.octets().as_ref());
    }
    message.finish()
}

/// 标准地址到 Linux rtmsg 的静态投影；Linux 族号仅属于此边界。
pub(super) trait RouteAddress: network_protocol::routes::RouteAddress {
    type Octets: AsRef<[u8]>;
    const LINUX_FAMILY: u8;
    fn octets(self) -> Self::Octets;
}
impl RouteAddress for core::net::Ipv4Addr {
    type Octets = [u8; 4];
    const LINUX_FAMILY: u8 = AF_INET;
    fn octets(self) -> Self::Octets {
        core::net::Ipv4Addr::octets(&self)
    }
}
impl RouteAddress for core::net::Ipv6Addr {
    type Octets = [u8; 16];
    const LINUX_FAMILY: u8 = AF_INET6;
    fn octets(self) -> Self::Octets {
        core::net::Ipv6Addr::octets(&self)
    }
}

/// 冻结组的 Linux 表示：只保存长度与精确权重公因数，不复制成员或建立逐路由对象。
struct GroupEncoding {
    attribute_length: u16,
    weight_divisor: u32,
}

fn gateway_attribute<A: RouteAddress>(gateway: A::Gateway) -> (u16, [u8; 18], usize) {
    let mut bytes = [0; 18];
    match gateway.into() {
        core::net::IpAddr::V4(address) => {
            bytes[..4].copy_from_slice(&address.octets());
            (RTA_GATEWAY, bytes, 4)
        }
        core::net::IpAddr::V6(address) if A::LINUX_FAMILY == AF_INET6 => {
            bytes[..16].copy_from_slice(&address.octets());
            (RTA_GATEWAY, bytes, 16)
        }
        core::net::IpAddr::V6(address) => {
            bytes[..2].copy_from_slice(&u16::from(AF_INET6).to_ne_bytes());
            bytes[2..].copy_from_slice(&address.octets());
            (RTA_VIA, bytes, 18)
        }
    }
}

/// Linux dump 的唯一捕获正文和每组表示；布局只由不可变正文推导，发布后无需人工同步。
pub(super) struct RouteDump<A> {
    snapshot: RoutingSnapshot<A>,
    groups: Vec<GroupEncoding>,
    maximum_message_size: usize,
}
impl<A: RouteAddress> RouteDump<A> {
    /// native 权重为 u32；Linux rtnh_hops 仅有八位。先约分，不能精确表示则 EOVERFLOW；
    /// 完整 RTA_MULTIPATH 超出 u16 长度则 EMSGSIZE，均不截断或修改执行中的 FIB。
    pub(super) fn new(snapshot: RoutingSnapshot<A>) -> Result<Self, i32> {
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
                let gateway_length = member
                    .gateway
                    .map_or(0, |gateway| align4(4 + gateway_attribute::<A>(gateway).2));
                length = length.checked_add(8 + gateway_length).ok_or(-EMSGSIZE)?;
                divisor = gcd(divisor, member.weight);
            }
            let attribute_length = if group.len() > 1 {
                let length = u16::try_from(length).map_err(|_| -EMSGSIZE)?;
                if group.members().any(|member| member.weight / divisor > 256) {
                    return Err(-EOVERFLOW);
                }
                // 固定部分至多 104 + 3×(地址宽度−4)：目的、网关和首选源；不低估 IPv6 输出预留。
                maximum_message_size =
                    maximum_message_size.max(104 + 3 * (A::BYTES - 4) + usize::from(length));
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
                let gateway = member.gateway.map(gateway_attribute::<A>);
                let length =
                    (8 + gateway.as_ref().map_or(0, |(_, _, size)| align4(4 + size))) as u16;
                out.extend_from_slice(&length.to_ne_bytes());
                out.push(if member.on_link {
                    RTNH_F_ONLINK as u8
                } else {
                    0
                });
                out.push((member.weight / layout.weight_divisor - 1) as u8);
                out.extend_from_slice(&(member.interface_index as i32).to_ne_bytes());
                if let Some((kind, bytes, size)) = gateway {
                    out.extend_from_slice(&((4 + size) as u16).to_ne_bytes());
                    out.extend_from_slice(&kind.to_ne_bytes());
                    out.extend_from_slice(&bytes[..size]);
                    out.resize(out.len() + align4(size) - size, 0);
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
fn route_header<A: RouteAddress>(
    row: RouteSnapshot<'_, A>,
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
        A::LINUX_FAMILY,
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
        message.attribute(RTA_DST, route.destination.octets().as_ref());
    }
    if let Some(index) = single.and_then(|route| route.interface_index) {
        message.attribute_u32(RTA_OIF, index);
    }
    if let Some(gateway) = single.and_then(|route| route.gateway) {
        let (kind, bytes, size) = gateway_attribute::<A>(gateway);
        message.attribute(kind, &bytes[..size]);
    }
    if let Some(source) = route.preferred_source {
        message.attribute(RTA_PREFSRC, source.octets().as_ref());
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

/// Linux ifindex 是有符号正整数；在发布 dump 前检查整批，不把原生 u32 身份截成负设备号。
pub(super) fn validate_neighbors(rows: &network_protocol::NeighborSnapshot) -> Result<(), i32> {
    if (0..rows.len())
        .filter_map(|index| rows.get(index))
        .any(|row| i32::try_from(row.interface_index).is_err())
    {
        return Err(EOVERFLOW);
    }
    Ok(())
}

/// 冻结的 ARP/ND 事实投影为 ndmsg；地址宽度与地址族一致。
pub(super) fn neighbor(
    row: network_protocol::Neighbor,
    flags: u16,
    sequence: u32,
    port: u32,
) -> ([u8; MESSAGE_CAPACITY], usize) {
    use network_protocol::NeighborState;
    let mut message = Message::new(RTM_NEWNEIGH, flags, sequence, port);
    let state: u16 = match row.state {
        NeighborState::Incomplete => 1,
        NeighborState::Reachable => 2,
        NeighborState::Stale => 4,
        NeighborState::Delay => 8,
        NeighborState::Probe => 16,
        NeighborState::Failed => 32,
    };
    let mut header = [0; 12];
    header[0] = if row.address.is_ipv4() { 2 } else { 10 };
    header[4..8].copy_from_slice(
        &i32::try_from(row.interface_index)
            .expect("Linux neighbor snapshot prevalidated")
            .to_ne_bytes(),
    );
    header[8..10].copy_from_slice(&state.to_ne_bytes());
    header[10] = if row.is_router { 0x80 } else { 0 };
    header[11] = 1; // RTN_UNICAST；协议 owner 不提供 proxy/bridge FDB 条目。
    message.put(&header);
    match row.address {
        core::net::IpAddr::V4(address) => message.attribute(1, &address.octets()),
        core::net::IpAddr::V6(address) => message.attribute(1, &address.octets()),
    }
    if let Some(mac) = row.hardware_address {
        message.attribute(2, &mac);
    }
    if matches!(row.state, NeighborState::Incomplete | NeighborState::Probe) {
        message.attribute_u32(4, u32::from(row.probes));
    }
    message.finish()
}

/// 普通 dump 沿 Linux 非 strict 请求语义；过滤接口来自 NDA_IFINDEX，而非 ndm_ifindex。
/// master/FDB 的过滤必须由实际网桥 owner 提供，不能静默返回另一集合。
pub(super) fn neighbor_dump(payload: &[u8]) -> Result<(u32, bool), i32> {
    if payload.len() < 12 {
        return Ok((0, false));
    }
    let proxy = payload[10] == 8;
    // 非 strict Linux dump 忽略解析错误，且不会发布尚未完成的过滤条件。
    let Ok(attributes) = validated_attributes(payload, 12) else {
        return Ok((0, proxy));
    };
    let mut index = 0;
    let mut master = 0;
    for (kind, value) in attributes {
        if matches!(kind, 8 | 9) && value.len() < 4 {
            return Ok((0, proxy));
        }
        if value.len() >= 4 {
            let value = u32::from_ne_bytes(value[..4].try_into().expect("attribute width"));
            match kind {
                8 => index = value,
                9 => master = value,
                _ => {}
            }
        }
    }
    if master != 0 {
        return Err(95);
    }
    Ok((index, payload[10] == 8))
}

/// 标准单邻居 GET 的严格头/属性检查；不触发解析，不接受写入或 proxy 控制标记。
pub(super) fn neighbor_get(payload: &[u8]) -> Result<(u32, core::net::IpAddr, bool), i32> {
    let header = payload.get(..12).ok_or(22)?;
    if header[1..4] != [0; 3] || header[8..10] != [0; 2] || header[10] & !8 != 0 || header[11] != 0
    {
        return Err(22);
    }
    let family = header[0];
    if !matches!(family, 2 | 10) {
        return Err(97);
    }
    let index = u32::from_ne_bytes(header[4..8].try_into().expect("ndmsg width"));
    let mut destination = None;
    for (kind, data) in validated_attributes(payload, 12)? {
        if kind != 1 {
            return Err(22);
        }
        destination = Some(if family == 2 {
            core::net::Ipv4Addr::from(<[u8; 4]>::try_from(data).map_err(|_| 22)?).into()
        } else {
            core::net::Ipv6Addr::from(<[u8; 16]>::try_from(data).map_err(|_| 22)?).into()
        });
    }
    let proxy = header[10] == 8;
    if index == 0 && !proxy {
        return Err(22);
    }
    Ok((index, destination.ok_or(22)?, proxy))
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

/// 在语义解析前验证整个属性区 framing；沿用同一 iterator，不另建 codec 或复制正文。
/// 最后一项可以省略尾部对齐字节，半个头或越界长度不能伪装成正常结束。
fn validated_attributes(
    payload: &[u8],
    skip: usize,
) -> Result<impl Iterator<Item = (u16, &[u8])>, i32> {
    if payload.len() < skip {
        return Err(22);
    }
    let end = attributes(payload, skip).try_fold(align4(skip), |offset, (_, value)| {
        offset.checked_add(align4(4 + value.len())).ok_or(22)
    })?;
    if end < payload.len() {
        return Err(22);
    }
    Ok(attributes(payload, skip))
}

/// 找 `kind` 属性的负载。
pub(super) fn find_attribute(payload: &[u8], skip: usize, kind: u16) -> Option<&[u8]> {
    attributes(payload, skip).find_map(|(found, data)| (found == kind).then_some(data))
}

/// 请求里用于按名字查询设备的属性类型。
pub(super) const IFLA_IFNAME_ATTRIBUTE: u16 = IFLA_IFNAME;
