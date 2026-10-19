//! NETLINK_ROUTE 修改请求：链路（MTU、UP/DOWN）、IPv4 地址与 IPv4 路由。
//!
//! 每个入口先完整解析并校验请求，再调用既有的 `configure_*`；校验失败时不改变任何状态。
//! 本实现的设备模型一个接口只有一个 IPv4 地址：第二个不同地址、点对点 peer、地址别名 label、
//! 设备重命名/创建、blackhole/prohibit 路由与多路径等都明确返回 `EOPNOTSUPP`，不会静默忽略。

use core::net::Ipv4Addr;

use super::codec::{self, NLM_F_APPEND, NLM_F_CREATE, NLM_F_EXCL, NLM_F_REPLACE};
use crate::socket::{
    InterfaceSnapshot, Ipv4RouteError, Ipv4RouteKind, Ipv4RouteMetrics, Ipv4RouteRequest,
    NetNamespace, SocketError, configure_address, configure_broadcast, configure_mtu,
    configure_netmask, configure_route, configure_up, interface_snapshot, interface_snapshots,
};

pub(super) const EPERM: i32 = 1;
const ENODEV: i32 = 19;
const EEXIST: i32 = 17;
const EINVAL: i32 = 22;
const ENOMEM: i32 = 12;
const ENOBUFS: i32 = 105;
const EADDRNOTAVAIL: i32 = 99;
const EAFNOSUPPORT: i32 = 97;
pub(super) const EOPNOTSUPP: i32 = 95;

const AF_INET: u8 = 2;
const IFF_UP: u32 = 0x1;
/// Linux `dev_change_flags` 允许用户改变的位（`IFF_DEBUG|NOTRAILERS|NOARP|PROMISC|ALLMULTI|MULTICAST|…|UP`）。
const CHANGEABLE_FLAGS: u32 =
    0x1 | 0x4 | 0x20 | 0x80 | 0x100 | 0x200 | 0x1000 | 0x2000 | 0x4000 | 0x8000;

const IFLA_ADDRESS: u16 = 1;
const IFLA_MTU: u16 = 4;
const IFLA_IFNAME: u16 = 3;
const IFA_ADDRESS: u16 = 1;
const IFA_LOCAL: u16 = 2;
const IFA_LABEL: u16 = 3;
const IFA_BROADCAST: u16 = 4;
const IFA_FLAGS: u16 = 8;
const IFA_CACHEINFO: u16 = 6;
const RTA_DST: u16 = 1;
const RTA_OIF: u16 = 4;
const RTA_GATEWAY: u16 = 5;
const RTA_PRIORITY: u16 = 6;
const RTA_PREFSRC: u16 = 7;
const RTA_METRICS: u16 = 8;
const RTA_TABLE: u16 = 15;
const RTAX_WINDOW: u16 = 3;
const RTAX_RTT: u16 = 4;
const RTAX_ADVMSS: u16 = 8;
const RTNH_F_ONLINK: u32 = 4;
const RT_TABLE_MAIN: u32 = 254;

/// 把设备配置错误翻译成 Linux errno（正值）。
fn socket_errno(error: SocketError) -> i32 {
    match error {
        SocketError::NoMemory => ENOMEM,
        SocketError::NoBuffer => ENOBUFS,
        SocketError::AddressNotAvailable => EADDRNOTAVAIL,
        SocketError::NoDevice | SocketError::DeviceNotAvailable => ENODEV,
        _ => EINVAL,
    }
}

fn ipv4(data: &[u8]) -> Result<Ipv4Addr, i32> {
    <[u8; 4]>::try_from(data)
        .map(Ipv4Addr::from)
        .map_err(|_| EINVAL)
}

fn u32_value(data: &[u8]) -> Result<u32, i32> {
    <[u8; 4]>::try_from(data)
        .map(u32::from_ne_bytes)
        .map_err(|_| EINVAL)
}

fn interface_name(snapshot: &InterfaceSnapshot) -> &[u8] {
    let end = snapshot.name.iter().position(|b| *b == 0).unwrap_or(16);
    &snapshot.name[..end]
}

/// `RTM_NEWLINK`/`RTM_SETLINK`：修改已有设备的 MTU 与 UP 状态。
///
/// # Parameters
///
/// - `scope`: 目标 NET。
/// - `request_flags`: nlmsg_flags，用于 `NLM_F_CREATE/EXCL/REPLACE`。
/// - `payload`: `ifinfomsg` 及其属性。
pub(super) fn link(scope: &NetNamespace, request_flags: u16, payload: &[u8]) -> Result<(), i32> {
    if payload.len() < 16 {
        return Err(EINVAL);
    }
    let index = i32::from_ne_bytes(payload[4..8].try_into().expect("ifi_index"));
    let flags = u32::from_ne_bytes(payload[8..12].try_into().expect("ifi_flags"));
    let change = u32::from_ne_bytes(payload[12..16].try_into().expect("ifi_change"));
    let name = codec::find_attribute(payload, 16, IFLA_IFNAME)
        .map(|name| name.split(|byte| *byte == 0).next().unwrap_or(name));
    let interfaces = interface_snapshots(scope).map_err(socket_errno)?;
    let target = if index > 0 {
        interfaces.iter().find(|row| row.index == index as u32)
    } else if let Some(name) = name {
        interfaces.iter().find(|row| interface_name(row) == name)
    } else {
        return Err(EINVAL);
    };
    let Some(target) = target else {
        // 创建新设备（veth、bridge…）不在本实现范围。
        return Err(if request_flags & NLM_F_CREATE != 0 {
            EOPNOTSUPP
        } else {
            ENODEV
        });
    };
    if request_flags & NLM_F_EXCL != 0 {
        return Err(EEXIST);
    }
    if request_flags & NLM_F_REPLACE != 0 {
        return Err(EOPNOTSUPP);
    }

    // 1. 先校验全部属性：只有 MTU 可改；重命名与其他属性明确不支持。
    let mut mtu = None;
    for (kind, data) in codec::attributes(payload, 16) {
        match kind {
            IFLA_IFNAME
                if index > 0
                    && Some(data.split(|b| *b == 0).next().unwrap_or(data))
                        != Some(interface_name(target)) =>
            {
                return Err(EOPNOTSUPP);
            }
            IFLA_IFNAME => {}
            IFLA_MTU => mtu = Some(u32_value(data)?),
            IFLA_ADDRESS if data == target.mac => {}
            _ => return Err(EOPNOTSUPP),
        }
    }
    // 2. ifi_flags/ifi_change 的 Linux 语义：change 为 0 时 flags 整体替换，否则只替换 change 选中的位。
    let current = u32::from(target.flags());
    let desired = if flags == 0 && change == 0 {
        current
    } else if change != 0 {
        (flags & change) | (current & !change)
    } else {
        flags
    };
    let difference = (desired ^ current) & CHANGEABLE_FLAGS;
    if difference & !IFF_UP != 0 {
        // 混杂、全组播、关闭 multicast 等在本设备模型里没有对应状态。
        return Err(EOPNOTSUPP);
    }
    // 3. 应用：先 MTU，后 UP/DOWN（与 Linux do_setlink 顺序一致）。
    if let Some(mtu) = mtu {
        configure_mtu(scope, target.index, i32::try_from(mtu).map_err(|_| EINVAL)?)
            .map_err(socket_errno)?;
    }
    if difference & IFF_UP != 0 {
        configure_up(scope, target.index, desired & IFF_UP != 0).map_err(socket_errno)?;
    }
    Ok(())
}

fn netmask(prefix: u8) -> Ipv4Addr {
    Ipv4Addr::from(u32::MAX.checked_shl(32 - u32::from(prefix)).unwrap_or(0))
}

/// 解析 `ifaddrmsg` 头：返回 `(prefix_length, interface_index)`。
fn address_header(payload: &[u8]) -> Result<(u8, u32), i32> {
    if payload.len() < 8 {
        return Err(EINVAL);
    }
    if payload[0] != AF_INET {
        return Err(EAFNOSUPPORT);
    }
    if payload[1] > 32 {
        return Err(EINVAL);
    }
    Ok((
        payload[1],
        u32::from_ne_bytes(payload[4..8].try_into().expect("ifa_index")),
    ))
}

/// `RTM_NEWADDR`：给设备设置 IPv4 地址、前缀与广播地址。
pub(super) fn address_new(
    scope: &NetNamespace,
    request_flags: u16,
    payload: &[u8],
) -> Result<(), i32> {
    let (prefix, index) = address_header(payload)?;
    let mut local = None;
    let mut peer = None;
    let mut broadcast = None;
    let mut label = None;
    for (kind, data) in codec::attributes(payload, 8) {
        match kind {
            IFA_LOCAL => local = Some(ipv4(data)?),
            IFA_ADDRESS => peer = Some(ipv4(data)?),
            IFA_BROADCAST => broadcast = Some(ipv4(data)?),
            IFA_LABEL => label = Some(data.split(|b| *b == 0).next().unwrap_or(data)),
            // 生存期与标志不改变本模型里的永久地址语义。
            IFA_FLAGS | IFA_CACHEINFO => {}
            _ => return Err(EOPNOTSUPP),
        }
    }
    let local = local.ok_or(EINVAL)?;
    // IFA_ADDRESS 与 IFA_LOCAL 不同是点对点 peer 地址。
    if peer.is_some_and(|peer| peer != local) {
        return Err(EOPNOTSUPP);
    }
    let snapshot = interface_snapshot(scope, index).map_err(socket_errno)?;
    // label 必须等于设备名；"eth0:1" 这样的别名意味着第二个地址。
    if label.is_some_and(|label| label != interface_name(&snapshot)) {
        return Err(EOPNOTSUPP);
    }
    match snapshot.address {
        Some(existing) if existing != local => return Err(EOPNOTSUPP),
        Some(_) if request_flags & NLM_F_EXCL != 0 => return Err(EEXIST),
        _ => {}
    }
    configure_address(scope, index, local).map_err(socket_errno)?;
    configure_netmask(scope, index, netmask(prefix)).map_err(socket_errno)?;
    if let Some(broadcast) = broadcast {
        configure_broadcast(scope, index, broadcast).map_err(socket_errno)?;
    }
    Ok(())
}

/// `RTM_DELADDR`：删除设备的 IPv4 地址；地址或前缀不匹配得到 `EADDRNOTAVAIL`。
pub(super) fn address_delete(scope: &NetNamespace, payload: &[u8]) -> Result<(), i32> {
    let (prefix, index) = address_header(payload)?;
    let mut local = None;
    let mut address = None;
    for (kind, data) in codec::attributes(payload, 8) {
        match kind {
            IFA_LOCAL => local = Some(ipv4(data)?),
            IFA_ADDRESS => address = Some(ipv4(data)?),
            IFA_BROADCAST | IFA_LABEL | IFA_FLAGS | IFA_CACHEINFO => {}
            _ => return Err(EOPNOTSUPP),
        }
    }
    let wanted = local.or(address).ok_or(EINVAL)?;
    let snapshot = interface_snapshot(scope, index).map_err(socket_errno)?;
    if snapshot.address != Some(wanted) || prefix != 0 && prefix != snapshot.prefix_length {
        return Err(EADDRNOTAVAIL);
    }
    configure_address(scope, index, Ipv4Addr::UNSPECIFIED).map_err(socket_errno)
}

fn route_kind(kind: u8) -> Result<Ipv4RouteKind, i32> {
    match kind {
        1 => Ok(Ipv4RouteKind::Unicast),
        2 => Ok(Ipv4RouteKind::Local),
        3 => Ok(Ipv4RouteKind::Broadcast),
        7 => Ok(Ipv4RouteKind::Unreachable),
        9 => Ok(Ipv4RouteKind::Throw),
        // anycast、multicast、blackhole、prohibit、nat：FIB 没有对应语义。
        4 | 5 | 6 | 8 | 10 => Err(EOPNOTSUPP),
        _ => Err(EINVAL),
    }
}

/// `RTM_NEWROUTE`/`RTM_DELROUTE`：进入同一 NET 的唯一 FIB。
///
/// # Parameters
///
/// - `delete`: `RTM_DELROUTE`。
/// - `request_flags`: `NLM_F_CREATE/EXCL/REPLACE/APPEND` 原样传给 FIB。
pub(super) fn route(
    scope: &NetNamespace,
    delete: bool,
    request_flags: u16,
    payload: &[u8],
) -> Result<(), i32> {
    if payload.len() < 12 {
        return Err(EINVAL);
    }
    if payload[0] != AF_INET {
        return Err(EAFNOSUPPORT);
    }
    let prefix_length = payload[1];
    // 源地址路由与 TOS 选择在 FIB 里没有对应维度。
    if payload[2] != 0 || payload[3] != 0 {
        return Err(EOPNOTSUPP);
    }
    let mut table = u32::from(payload[4]);
    let protocol = payload[5];
    let scope_value = payload[6];
    let kind = payload[7];
    let route_flags = u32::from_ne_bytes(payload[8..12].try_into().expect("rtm_flags"));
    let mut request = Ipv4RouteRequest {
        destination: Ipv4Addr::UNSPECIFIED,
        prefix_length,
        table: 0,
        metric: 0,
        scope: scope_value,
        protocol,
        kind: Ipv4RouteKind::Unicast,
        interface_index: None,
        gateway: None,
        preferred_source: None,
        on_link: route_flags & RTNH_F_ONLINK != 0,
        metrics: Ipv4RouteMetrics::default(),
    };
    for (attribute, data) in codec::attributes(payload, 12) {
        match attribute {
            RTA_DST => request.destination = ipv4(data)?,
            RTA_OIF => request.interface_index = Some(u32_value(data)?),
            RTA_GATEWAY => request.gateway = Some(ipv4(data)?),
            RTA_PRIORITY => request.metric = u32_value(data)?,
            RTA_PREFSRC => request.preferred_source = Some(ipv4(data)?),
            RTA_TABLE => table = u32_value(data)?,
            RTA_METRICS => {
                for (metric, value) in codec::attributes(data, 0) {
                    match metric {
                        RTAX_WINDOW => request.metrics.receive_window = u32_value(value)?,
                        RTAX_RTT => request.metrics.round_trip_time = u32_value(value)?,
                        RTAX_ADVMSS => request.metrics.advertised_mss = u32_value(value)?,
                        _ => return Err(EOPNOTSUPP),
                    }
                }
            }
            // 多路径、源路由、encap、flow 等没有对应语义。
            _ => return Err(EOPNOTSUPP),
        }
    }
    request.table = if table == 0 { RT_TABLE_MAIN } else { table };
    let replace = request_flags & NLM_F_REPLACE != 0;
    let create = request_flags & NLM_F_CREATE != 0;
    let exclusive = request_flags & NLM_F_EXCL != 0;
    let append = request_flags & NLM_F_APPEND != 0;
    if !delete {
        // 新增必须指明类型；RTN_UNSPEC 只在删除时表示“任意类型”。
        request.kind = route_kind(kind)?;
        return configure_route(scope, request, false, replace, create, exclusive, append)
            .map_err(|error| error.errno());
    }
    if kind != 0 {
        request.kind = route_kind(kind)?;
        return configure_route(scope, request, true, false, false, false, false)
            .map_err(|error| error.errno());
    }
    // RTN_UNSPEC 删除：FIB 按类型精确匹配，依次尝试每种类型，全部不存在才算 ESRCH。
    for candidate in [
        Ipv4RouteKind::Unicast,
        Ipv4RouteKind::Local,
        Ipv4RouteKind::Broadcast,
        Ipv4RouteKind::Unreachable,
        Ipv4RouteKind::Throw,
    ] {
        request.kind = candidate;
        match configure_route(scope, request, true, false, false, false, false) {
            Err(Ipv4RouteError::NotFound) => {}
            other => return other.map_err(|error| error.errno()),
        }
    }
    Err(Ipv4RouteError::NotFound.errno())
}
