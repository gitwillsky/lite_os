use super::{
    ARPHRD_ETHER, ETHERNET_MTU, IFF_BROADCAST, IFF_MULTICAST, IFF_RUNNING, IFF_UP, IFNAMSIZ,
    IFREQ_SIZE, INTERFACE_INDEX, SIOCADDRT, SIOCDELRT, SIOCGIFADDR, SIOCGIFBRDADDR, SIOCGIFCONF,
    SIOCGIFFLAGS, SIOCGIFHWADDR, SIOCGIFINDEX, SIOCGIFMTU, SIOCGIFNAME, SIOCGIFNETMASK,
    SIOCSIFADDR, SIOCSIFBRDADDR, SIOCSIFFLAGS, SIOCSIFMTU, SIOCSIFNETMASK, Socket, SocketDomain,
    TaskControlBlock, broadcast, configure_address, configure_gateway, configure_netmask,
    configure_up, decode_inet_sockaddr, encode_inet_sockaddr, encode_interface_name, errno,
    interface_name_matches, interface_snapshot, netmask, socket_error,
};

const RTENTRY_SIZE: usize = 120;
const RTF_UP: u16 = 0x1;
const RTF_GATEWAY: u16 = 0x2;

fn configure_route(task: &TaskControlBlock, request: usize, argument: usize) -> isize {
    if argument == 0 {
        return -errno::EFAULT;
    }
    let mut route = [0u8; RTENTRY_SIZE];
    if task.copy_from_user(argument, &mut route).is_err() {
        return -errno::EFAULT;
    }
    let family = u16::from_ne_bytes(route[8..10].try_into().unwrap()) as usize;
    let gateway_family = u16::from_ne_bytes(route[24..26].try_into().unwrap()) as usize;
    let mask_family = u16::from_ne_bytes(route[40..42].try_into().unwrap()) as usize;
    let destination = core::net::Ipv4Addr::from(<[u8; 4]>::try_from(&route[12..16]).unwrap());
    let gateway = core::net::Ipv4Addr::from(<[u8; 4]>::try_from(&route[28..32]).unwrap());
    let mask = core::net::Ipv4Addr::from(<[u8; 4]>::try_from(&route[44..48]).unwrap());
    let flags = u16::from_ne_bytes(route[56..58].try_into().unwrap());
    if family != super::AF_INET
        || !matches!(mask_family, 0 | super::AF_INET)
        || !destination.is_unspecified()
        || !mask.is_unspecified()
    {
        return -errno::EOPNOTSUPP;
    }
    let result = if request == SIOCADDRT {
        if gateway_family != super::AF_INET
            || flags & (RTF_UP | RTF_GATEWAY) != (RTF_UP | RTF_GATEWAY)
        {
            return -errno::EINVAL;
        }
        configure_gateway(Some(gateway))
    } else {
        configure_gateway(None)
    };
    result.map_or_else(socket_error, |()| 0)
}

fn copy_ifconf(task: &TaskControlBlock, argument: usize) -> isize {
    if argument == 0 {
        return -errno::EFAULT;
    }
    let mut ifconf = [0u8; 16];
    if task.copy_from_user(argument, &mut ifconf).is_err() {
        return -errno::EFAULT;
    }
    let capacity = i32::from_ne_bytes(ifconf[..4].try_into().unwrap()).max(0) as usize;
    let buffer = usize::from_ne_bytes(ifconf[8..16].try_into().unwrap());
    let required = IFREQ_SIZE;
    if buffer != 0 && capacity >= required {
        let snapshot = match interface_snapshot() {
            Ok(value) => value,
            Err(error) => return socket_error(error),
        };
        let mut request = [0u8; IFREQ_SIZE];
        encode_interface_name(&mut request);
        encode_inet_sockaddr(
            &mut request,
            snapshot.address.unwrap_or(core::net::Ipv4Addr::UNSPECIFIED),
        );
        if task.copy_to_user(buffer, &request).is_err() {
            return -errno::EFAULT;
        }
    }
    ifconf[..4].copy_from_slice(&(required as i32).to_ne_bytes());
    task.copy_to_user(argument, &ifconf)
        .map_or(-errno::EFAULT, |()| 0)
}

/// @description 实现 BusyBox/标准工具消费的 Linux AF_INET interface ioctl ABI。
///
/// @param task 当前 address-space owner，仅用于 ifreq copyin/copyout。
/// @param socket 发起 ioctl 的 OFD socket facade，必须属于 AF_INET。
/// @param request Linux SIOC request number。
/// @param argument `ifreq` 或 `ifconf` userspace pointer。
/// @return 配置/查询成功返回零；address、name、request 或 user-copy 错误返回负 errno。
pub(in crate::syscall) fn socket_ioctl(
    task: &TaskControlBlock,
    socket: &Socket,
    request: usize,
    argument: usize,
) -> isize {
    if socket.domain() != SocketDomain::Inet {
        return -errno::ENOTTY;
    }
    if matches!(request, SIOCADDRT | SIOCDELRT) {
        return configure_route(task, request, argument);
    }
    if request == SIOCGIFCONF {
        return copy_ifconf(task, argument);
    }
    if argument == 0 {
        return -errno::EFAULT;
    }
    let mut ifreq = [0u8; IFREQ_SIZE];
    if task.copy_from_user(argument, &mut ifreq).is_err() {
        return -errno::EFAULT;
    }
    if request == SIOCGIFNAME {
        if i32::from_ne_bytes(ifreq[16..20].try_into().unwrap()) != INTERFACE_INDEX {
            return -errno::ENODEV;
        }
        ifreq[..IFNAMSIZ].fill(0);
        encode_interface_name(&mut ifreq);
    } else if !interface_name_matches(&ifreq) {
        return -errno::ENODEV;
    }
    let snapshot = match interface_snapshot() {
        Ok(value) => value,
        Err(error) => return socket_error(error),
    };
    let result: Result<(), isize> = match request {
        SIOCGIFNAME => Ok(()),
        SIOCGIFFLAGS => {
            let flags =
                IFF_BROADCAST | IFF_MULTICAST | if snapshot.up { IFF_UP | IFF_RUNNING } else { 0 };
            ifreq[16..18].copy_from_slice(&flags.to_ne_bytes());
            Ok(())
        }
        SIOCSIFFLAGS => {
            let flags = u16::from_ne_bytes(ifreq[16..18].try_into().unwrap());
            configure_up(flags & IFF_UP != 0).map_err(socket_error)
        }
        SIOCGIFADDR => {
            encode_inet_sockaddr(
                &mut ifreq,
                snapshot.address.unwrap_or(core::net::Ipv4Addr::UNSPECIFIED),
            );
            Ok(())
        }
        SIOCSIFADDR => decode_inet_sockaddr(&ifreq)
            .and_then(|address| configure_address(address).map_err(socket_error)),
        SIOCGIFNETMASK => {
            encode_inet_sockaddr(&mut ifreq, netmask(snapshot.prefix_length));
            Ok(())
        }
        SIOCSIFNETMASK => decode_inet_sockaddr(&ifreq)
            .and_then(|mask| configure_netmask(mask).map_err(socket_error)),
        SIOCGIFBRDADDR => match snapshot.address {
            Some(address) => {
                encode_inet_sockaddr(&mut ifreq, broadcast(address, snapshot.prefix_length));
                Ok(())
            }
            None => Err(-errno::EADDRNOTAVAIL),
        },
        SIOCSIFBRDADDR => match (decode_inet_sockaddr(&ifreq), snapshot.address) {
            (Ok(requested), Some(address))
                if requested == broadcast(address, snapshot.prefix_length) =>
            {
                Ok(())
            }
            (Err(error), _) => Err(error),
            (_, None) => Err(-errno::EADDRNOTAVAIL),
            _ => Err(-errno::EINVAL),
        },
        SIOCGIFMTU => {
            ifreq[16..20].copy_from_slice(&ETHERNET_MTU.to_ne_bytes());
            Ok(())
        }
        SIOCSIFMTU => {
            if i32::from_ne_bytes(ifreq[16..20].try_into().unwrap()) == ETHERNET_MTU {
                Ok(())
            } else {
                Err(-errno::EINVAL)
            }
        }
        SIOCGIFHWADDR => {
            ifreq[16..32].fill(0);
            ifreq[16..18].copy_from_slice(&ARPHRD_ETHER.to_ne_bytes());
            ifreq[18..24].copy_from_slice(&snapshot.mac);
            Ok(())
        }
        SIOCGIFINDEX => {
            ifreq[16..20].copy_from_slice(&INTERFACE_INDEX.to_ne_bytes());
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
