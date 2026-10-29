use super::{
    ARPHRD_ETHER, IFF_UP, IFNAMSIZ, IFREQ_SIZE, SIOCADDRT, SIOCDELRT, SIOCGIFADDR, SIOCGIFBRDADDR,
    SIOCGIFCONF, SIOCGIFFLAGS, SIOCGIFHWADDR, SIOCGIFINDEX, SIOCGIFMTU, SIOCGIFNAME,
    SIOCGIFNETMASK, SIOCSIFADDR, SIOCSIFBRDADDR, SIOCSIFFLAGS, SIOCSIFMTU, SIOCSIFNETMASK, Socket,
    SocketDomain, TaskControlBlock, configure_address, configure_broadcast, configure_mtu,
    configure_netmask, configure_route as configure_ipv4_route, configure_up, decode_inet_sockaddr,
    encode_inet_sockaddr, errno, interface_index, interface_snapshot, interface_snapshots, netmask,
    socket_error,
};

const RTENTRY_SIZE: usize = 120;
const RTF_GATEWAY: u16 = 0x2;
const RTF_HOST: u16 = 0x4;
const RTF_MTU: u16 = 0x40;
const RTF_WINDOW: u16 = 0x80;
const RTF_IRTT: u16 = 0x100;
const RTF_REJECT: u16 = 0x200;

fn route_error(error: crate::net::Ipv4RouteError) -> isize {
    -(error.errno() as isize)
}
fn configure_route(
    task: &TaskControlBlock,
    scope: &crate::net::NetNamespace,
    operation: usize,
    argument: usize,
) -> isize {
    use crate::net::{Ipv4RouteKind, Ipv4RouteMetrics, Ipv4RouteRequest};
    if argument == 0 {
        return -errno::EFAULT;
    }
    let mut route = [0u8; RTENTRY_SIZE];
    if task.copy_from_user(argument, &mut route).is_err() {
        return -errno::EFAULT;
    }
    if !task.has_namespace_capability(
        &scope.user_namespace(),
        syscall_abi::capability::Capability::NET_ADMIN,
    ) {
        return -errno::EPERM;
    }
    let family = u16::from_ne_bytes(route[8..10].try_into().unwrap()) as usize;
    let gateway_family = u16::from_ne_bytes(route[24..26].try_into().unwrap()) as usize;
    let mask_family = u16::from_ne_bytes(route[40..42].try_into().unwrap()) as usize;
    let destination = core::net::Ipv4Addr::from(<[u8; 4]>::try_from(&route[12..16]).unwrap());
    let gateway = core::net::Ipv4Addr::from(<[u8; 4]>::try_from(&route[28..32]).unwrap());
    let mask = u32::from_be_bytes(route[44..48].try_into().unwrap());
    let flags = u16::from_ne_bytes(route[56..58].try_into().unwrap());
    if family != super::AF_INET {
        return -errno::EAFNOSUPPORT;
    }
    let prefix_length = if flags & RTF_HOST != 0 {
        32
    } else {
        if mask_family != super::AF_INET && !(mask_family == 0 && mask == 0) {
            return -errno::EAFNOSUPPORT;
        }
        let prefix = mask.leading_ones() as u8;
        if mask != u32::MAX.checked_shl(32 - u32::from(prefix)).unwrap_or(0) {
            return -errno::EINVAL;
        }
        prefix
    };
    let prefix_mask = u32::MAX
        .checked_shl(32 - u32::from(prefix_length))
        .unwrap_or(0);
    if u32::from(destination) & prefix_mask != u32::from(destination) {
        return -errno::EINVAL;
    }
    let metric = i16::from_ne_bytes(route[80..82].try_into().unwrap());
    let rejecting = flags & RTF_REJECT != 0;
    let mut request = Ipv4RouteRequest {
        destination,
        prefix_length,
        table: 254,
        metric: if metric == 0 {
            0
        } else {
            (i32::from(metric) - 1) as u32
        },
        scope: if rejecting { 254 } else { 253 },
        protocol: 3,
        kind: if rejecting {
            Ipv4RouteKind::Unreachable
        } else {
            Ipv4RouteKind::Unicast
        },
        interface_index: None,
        gateway: None,
        preferred_source: None,
        on_link: false,
        metrics: Ipv4RouteMetrics::default(),
    };
    // native RTF_REJECT在读取rt_dev/gateway之前完成配置，不能额外解引用无关用户指针。
    if !rejecting {
        let device_pointer = usize::from_ne_bytes(route[88..96].try_into().unwrap());
        if device_pointer != 0 {
            let mut name = [0u8; IFNAMSIZ];
            if task
                .copy_from_user(device_pointer, &mut name[..IFNAMSIZ - 1])
                .is_err()
            {
                return -errno::EFAULT;
            }
            let end = name
                .iter()
                .position(|byte| *byte == 0 || *byte == b':')
                .unwrap_or(IFNAMSIZ - 1);
            request.interface_index = match interface_index(scope, &name[..end]) {
                Ok(index) => Some(index),
                Err(error) => return socket_error(error),
            };
        }
        if gateway_family == super::AF_INET && !gateway.is_unspecified() {
            request.gateway = Some(gateway);
            if flags & RTF_GATEWAY != 0 {
                request.scope = 0;
            }
        }
        if operation == SIOCADDRT && flags & RTF_GATEWAY != 0 && request.gateway.is_none() {
            return -errno::EINVAL;
        }
    }
    // ioctl MTU是RTAX_ADVMSS+40，不能修改设备MTU或限制发送方的PMTU缓存。
    // native put_rtax使用u32截断；underflow经ip_metrics_convert规范化而非EINVAL。
    if operation == SIOCADDRT && !rejecting {
        if flags & RTF_MTU != 0 {
            request.metrics.advertised_mss =
                u64::from_ne_bytes(route[96..104].try_into().unwrap()).wrapping_sub(40) as u32;
        }
        if flags & RTF_WINDOW != 0 {
            request.metrics.receive_window =
                u64::from_ne_bytes(route[104..112].try_into().unwrap()) as u32;
        }
        if flags & RTF_IRTT != 0 {
            request.metrics.round_trip_time =
                u32::from(u16::from_ne_bytes(route[112..114].try_into().unwrap())) << 3;
        }
    }
    configure_ipv4_route(
        scope,
        request,
        operation == SIOCDELRT,
        false,
        true,
        false,
        false,
    )
    .map_or_else(route_error, |()| 0)
}

fn copy_ifconf(
    task: &TaskControlBlock,
    scope: &crate::net::NetNamespace,
    argument: usize,
) -> isize {
    if argument == 0 {
        return -errno::EFAULT;
    }
    let mut ifconf = [0u8; 16];
    if task.copy_from_user(argument, &mut ifconf).is_err() {
        return -errno::EFAULT;
    }
    let capacity = i32::from_ne_bytes(ifconf[..4].try_into().unwrap()).max(0) as usize;
    let buffer = usize::from_ne_bytes(ifconf[8..16].try_into().unwrap());
    let snapshots = match interface_snapshots(scope) {
        Ok(value) => value,
        Err(error) => return socket_error(error),
    };
    let mut copied = 0usize;
    for snapshot in snapshots
        .into_iter()
        .filter(|snapshot| snapshot.address.is_some())
    {
        if buffer != 0 && copied + IFREQ_SIZE > capacity {
            break;
        }
        if buffer != 0 {
            let mut request = [0u8; IFREQ_SIZE];
            request[..IFNAMSIZ].copy_from_slice(&snapshot.name);
            encode_inet_sockaddr(&mut request, snapshot.address.unwrap());
            let Some(output) = buffer.checked_add(copied) else {
                return -errno::EFAULT;
            };
            if task.copy_to_user(output, &request).is_err() {
                return -errno::EFAULT;
            }
        }
        copied += IFREQ_SIZE;
    }
    ifconf[..4].copy_from_slice(&(copied as i32).to_ne_bytes());
    task.copy_to_user(argument, &ifconf)
        .map_or(-errno::EFAULT, |()| 0)
}

/// 实现 BusyBox/标准工具消费的 Linux AF_INET interface ioctl ABI。
///
/// # Parameters
///
/// - `task`: 当前address-space与重新授权的credential owner。
/// - `socket`: 发起ioctl的实际OFD；所有socket domain均查询其捕获的NET。
/// - `request`: Linux SIOC request number。
/// - `argument`: `ifreq` 或 `ifconf` userspace pointer。
///
/// # Returns
///
/// 配置/查询成功返回零；address、name、request 或 user-copy 错误返回负 errno。
pub(in crate::syscall) fn socket_ioctl(
    task: &TaskControlBlock,
    socket: &Socket,
    request: usize,
    argument: usize,
) -> isize {
    let net = socket.net_namespace();
    let scope = net.as_ref();
    if socket.domain() == SocketDomain::Inet && matches!(request, SIOCADDRT | SIOCDELRT) {
        return configure_route(task, scope, request, argument);
    }
    if request == SIOCGIFCONF {
        return copy_ifconf(task, scope, argument);
    }
    if (request >> 8) & 0xff != 0x89 {
        return -errno::ENOTTY;
    }
    if argument == 0 {
        return -errno::EFAULT;
    }
    let mut ifreq = [0u8; IFREQ_SIZE];
    if task.copy_from_user(argument, &mut ifreq).is_err() {
        return -errno::EFAULT;
    }
    // IP地址/路由属于INET proto ioctl；其他domain的generic fallback先copy ifreq，
    // 再由dev_ioctl拒绝，不能把AF_UNIX伪装为IPv4配置入口。
    if socket.domain() != SocketDomain::Inet
        && matches!(
            request,
            SIOCADDRT
                | SIOCDELRT
                | SIOCGIFADDR
                | SIOCSIFADDR
                | SIOCGIFNETMASK
                | SIOCSIFNETMASK
                | SIOCGIFBRDADDR
                | SIOCSIFBRDADDR
        )
    {
        return -errno::ENOTTY;
    }
    if !matches!(
        request,
        SIOCGIFNAME
            | SIOCGIFFLAGS
            | SIOCSIFFLAGS
            | SIOCGIFADDR
            | SIOCSIFADDR
            | SIOCGIFNETMASK
            | SIOCSIFNETMASK
            | SIOCGIFBRDADDR
            | SIOCSIFBRDADDR
            | SIOCGIFMTU
            | SIOCSIFMTU
            | SIOCGIFHWADDR
            | SIOCGIFINDEX
    ) {
        return -errno::ENOTTY;
    }
    // 原生dev_ioctl/devinet_ioctl在ifreq copy之后、name查找之前重新检查owning UserNS。
    // 旧socket、SCM传递及setns不授予配置权；mapped/global uid0也不能替代NET_ADMIN。
    if matches!(
        request,
        SIOCSIFFLAGS | SIOCSIFADDR | SIOCSIFNETMASK | SIOCSIFBRDADDR | SIOCSIFMTU
    ) && !task.has_namespace_capability(
        &scope.user_namespace(),
        syscall_abi::capability::Capability::NET_ADMIN,
    ) {
        return -errno::EPERM;
    }
    if matches!(request, SIOCSIFADDR | SIOCSIFNETMASK | SIOCSIFBRDADDR)
        && let Err(error) = decode_inet_sockaddr(&ifreq)
    {
        return error;
    }
    ifreq[IFNAMSIZ - 1] = 0;
    let index = if request == SIOCGIFNAME {
        let index = i32::from_ne_bytes(ifreq[16..20].try_into().unwrap());
        if index <= 0 {
            return -errno::ENODEV;
        }
        index as u32
    } else {
        let end = ifreq[..IFNAMSIZ]
            .iter()
            .position(|byte| *byte == 0 || *byte == b':')
            .unwrap_or(IFNAMSIZ);
        match interface_index(scope, &ifreq[..end]) {
            Ok(index) => index,
            Err(error) => return socket_error(error),
        }
    };
    let snapshot = match interface_snapshot(scope, index) {
        Ok(value) => value,
        Err(error) => return socket_error(error),
    };
    if request == SIOCGIFNAME {
        ifreq[..IFNAMSIZ].copy_from_slice(&snapshot.name);
    }
    let result: Result<(), isize> = match request {
        SIOCGIFNAME => Ok(()),
        SIOCGIFFLAGS => {
            ifreq[16..18].copy_from_slice(&snapshot.flags().to_ne_bytes());
            Ok(())
        }
        SIOCSIFFLAGS => {
            let flags = u16::from_ne_bytes(ifreq[16..18].try_into().unwrap());
            configure_up(scope, index, flags & IFF_UP != 0).map_err(socket_error)
        }
        SIOCGIFADDR => match snapshot.address {
            Some(address) => {
                encode_inet_sockaddr(&mut ifreq, address);
                Ok(())
            }
            None => Err(-errno::EADDRNOTAVAIL),
        },
        SIOCSIFADDR => decode_inet_sockaddr(&ifreq)
            .and_then(|address| configure_address(scope, index, address).map_err(socket_error)),
        SIOCGIFNETMASK => match snapshot.address {
            Some(_) => {
                encode_inet_sockaddr(&mut ifreq, netmask(snapshot.prefix_length));
                Ok(())
            }
            None => Err(-errno::EADDRNOTAVAIL),
        },
        SIOCSIFNETMASK => decode_inet_sockaddr(&ifreq)
            .and_then(|mask| configure_netmask(scope, index, mask).map_err(socket_error)),
        SIOCGIFBRDADDR => match snapshot.address {
            Some(_) => {
                encode_inet_sockaddr(&mut ifreq, snapshot.broadcast);
                Ok(())
            }
            None => Err(-errno::EADDRNOTAVAIL),
        },
        SIOCSIFBRDADDR => decode_inet_sockaddr(&ifreq)
            .and_then(|address| configure_broadcast(scope, index, address).map_err(socket_error)),
        SIOCGIFMTU => {
            ifreq[16..20].copy_from_slice(&snapshot.mtu.to_ne_bytes());
            Ok(())
        }
        SIOCSIFMTU => configure_mtu(
            scope,
            index,
            i32::from_ne_bytes(ifreq[16..20].try_into().unwrap()),
        )
        .map_err(socket_error),
        SIOCGIFHWADDR => {
            ifreq[16..32].fill(0);
            ifreq[16..18].copy_from_slice(
                &(if snapshot.loopback {
                    772u16
                } else {
                    ARPHRD_ETHER
                })
                .to_ne_bytes(),
            );
            ifreq[18..24].copy_from_slice(&snapshot.mac);
            Ok(())
        }
        SIOCGIFINDEX => {
            ifreq[16..20].copy_from_slice(&(snapshot.index as i32).to_ne_bytes());
            Ok(())
        }
        _ => Err(-errno::EOPNOTSUPP),
    };
    match result {
        Ok(())
            if matches!(
                request,
                SIOCGIFNAME
                    | SIOCGIFFLAGS
                    | SIOCGIFADDR
                    | SIOCGIFBRDADDR
                    | SIOCGIFNETMASK
                    | SIOCGIFMTU
                    | SIOCGIFHWADDR
                    | SIOCGIFINDEX
            ) =>
        {
            task.copy_to_user(argument, &ifreq)
                .map_or(-errno::EFAULT, |()| 0)
        }
        Ok(()) => 0,
        Err(error) => error,
    }
}
