//! AF_INET：协议状态（TCP/UDP/ICMP/RAW endpoint、接口与 FIB）属于 netstack。
//!
//! 1. 控制操作经 socket channel，send/receive/poll 经该 socket 的固定共享 SQ/CQ；netstack 不等待网络进展。
//!    Linux 阻塞、超时与信号中断由本层调用者经本地 wait source 完成；
//! 2. netstack 在就绪状态可能变化时置位 socket 的就绪 Event，分派线程据此唤醒本地 notification，
//!    等待者被唤醒后重新查询电平状态；
//! 3. 接口与路由的查询和配置经所在网络域的域 channel 转发。

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::net::Ipv4Addr;

use device_protocol::ActivityBinding;
pub(super) use network_protocol::InetProtocol;
use network_protocol::{
    InetOptionChange, InterfaceSnapshot, Ipv4RouteError, Ipv4RouteRequest, NetworkSnapshot,
    RemoteInetSocket, RouteChange, RoutingSnapshot, SocketEndpoint, TRANSFER_LIMIT,
};

use super::{
    InetAddress, InternetOption, InternetPacketInfo, InternetSendPacketInfo, NetNamespace,
    SocketError, SocketPollState, SocketWaitSource, netstack,
};
use crate::ipc::{PipeDirection, PipeEnd, ReceiveBuffer};

/// 一个 AF_INET socket 的本地端。
pub(super) struct InetSocket {
    remote: RemoteInetSocket,
    // 所在网络域；SO_BINDTODEVICE 的 CAP_NET_RAW 在它的 owning user namespace 中判断。
    namespace: Arc<NetNamespace>,
    // 流式 socket 的一次发送可只部分完成；datagram 必须整条发送。
    stream: bool,
    // OWNER: netstack 就绪 Event 在分派表中的登记；最后一次文件关闭时摘除，缺失时分派表随 socket 数增长。
    readiness: ActivityBinding,
    notify_read: Arc<PipeEnd>,
    notify_write: Arc<PipeEnd>,
}

impl InetSocket {
    /// 在 `namespace` 的 netstack 协议实例中创建 endpoint。
    ///
    /// # Errors
    ///
    /// 域没有协议实例、netstack 拒绝（缓冲区不足等）或就绪登记失败。
    pub(super) fn new(
        family: network_protocol::InetFamily,
        protocol: InetProtocol,
        notify: (Arc<PipeEnd>, Arc<PipeEnd>),
        namespace: Arc<NetNamespace>,
    ) -> Result<Arc<Self>, SocketError> {
        let endpoint = netstack::protocol(&namespace)?.socket(family, protocol)?;
        Self::attach(endpoint, protocol == InetProtocol::Tcp, notify, namespace)
    }

    /// 接上 netstack 交回的 endpoint；任一步失败时 channel 随之关闭，netstack 关闭该 endpoint。
    fn attach(
        endpoint: SocketEndpoint,
        stream: bool,
        notify: (Arc<PipeEnd>, Arc<PipeEnd>),
        namespace: Arc<NetNamespace>,
    ) -> Result<Arc<Self>, SocketError> {
        let readiness = netstack::bind_readiness(endpoint.readiness, &endpoint.channel, &notify.1)?;
        Arc::try_new(Self {
            remote: RemoteInetSocket::new(endpoint.channel)?,
            namespace,
            stream,
            readiness,
            notify_read: notify.0,
            notify_write: notify.1,
        })
        .map_err(|_| SocketError::NoMemory)
    }

    /// 最后一次文件关闭：netstack 关闭 endpoint（TCP 进入关闭握手），本地 notification 关闭。
    pub(super) fn close_description(&self) {
        self.remote.close();
        netstack::retire_readiness(&self.readiness);
        self.notify_read.close();
        self.notify_write.close();
    }

    pub(super) fn bind(&self, address: InetAddress) -> Result<(), SocketError> {
        self.remote.bind(address)
    }

    pub(super) fn connect(&self, peer: InetAddress) -> Result<(), SocketError> {
        self.remote.connect(peer)
    }

    pub(super) fn disconnect(&self) -> Result<(), SocketError> {
        self.remote.disconnect()
    }

    pub(super) fn address(&self) -> Result<InetAddress, SocketError> {
        self.remote.address()
    }

    pub(super) fn peer_address(&self) -> Result<InetAddress, SocketError> {
        self.remote.peer_address()
    }

    /// 流式 socket 一次至多传递 [`TRANSFER_LIMIT`] 字节（部分发送）；datagram 整条交给 netstack。
    pub(super) fn send_to(
        &self,
        input: &[u8],
        target: Option<InetAddress>,
        packet_info: impl ExactSizeIterator<Item = InternetSendPacketInfo>,
    ) -> Result<usize, SocketError> {
        let input = if self.stream {
            &input[..input.len().min(TRANSFER_LIMIT)]
        } else {
            input
        };
        if self.stream {
            self.remote.send_to(input, target, core::iter::empty())
        } else {
            self.remote.send_to(input, target, packet_info)
        }
    }

    /// 取回至多 `output.remaining()` 字节（单次不超过 [`TRANSFER_LIMIT`]）。
    ///
    /// # Returns
    ///
    /// 复制的字节数、消息完整长度、来源地址与当前选项请求的双族 packet-info 事实。
    pub(super) fn receive_error(
        &self,
        output: &mut ReceiveBuffer<'_>,
    ) -> Result<super::ReceivedMessage, SocketError> {
        let received = self.remote.receive_error(output.remaining())?;
        output
            .prepare_append(received.data.len())
            .map_err(|()| SocketError::NoMemory)?;
        let count = output.append(&received.data);
        Ok(super::ReceivedMessage {
            count,
            full_length: received.full_length,
            source: Some(super::SocketAddress::Inet(received.error.destination)),
            internet_error: Some(received.error),
            internet_packet_info: None,
            packet_information: None,
            rights: None,
            credentials: None,
        })
    }

    pub(super) fn set_receive_error(&self, ipv6: bool, enabled: bool) -> Result<(), SocketError> {
        self.remote
            .set_option(InetOptionChange::ReceiveError { ipv6, enabled })
    }

    pub(super) fn receive(
        &self,
        output: &mut ReceiveBuffer<'_>,
        peek: bool,
    ) -> Result<
        (
            usize,
            usize,
            Option<InetAddress>,
            Option<InternetPacketInfo>,
        ),
        SocketError,
    > {
        let received = self.remote.receive(output.remaining(), peek)?;
        output
            .prepare_append(received.data.len())
            .map_err(|()| SocketError::NoMemory)?;
        let count = output.append(&received.data);
        Ok((
            count,
            received.full_length,
            received.source,
            received.packet_info,
        ))
    }

    pub(super) fn is_listening(&self) -> Result<bool, SocketError> {
        self.remote.is_listening()
    }

    pub(super) fn listen(&self, backlog: usize) -> Result<(), SocketError> {
        self.remote.listen(backlog)
    }

    /// 取一个已建立的连接，作为同一网络域中的新 socket。
    ///
    /// # Errors
    ///
    /// 暂无连接为 `Again`；非 listener、netstack 资源不足或就绪登记失败返回对应错误。
    pub(super) fn accept(
        &self,
        notify: (Arc<PipeEnd>, Arc<PipeEnd>),
    ) -> Result<Arc<Self>, SocketError> {
        let endpoint = self.remote.accept()?;
        Self::attach(endpoint, true, notify, self.namespace.clone())
    }

    pub(super) fn connection_result(&self) -> Result<(), SocketError> {
        self.remote.connection_result()
    }

    pub(super) fn take_error(&self) -> Option<SocketError> {
        self.remote.take_error()
    }

    pub(super) fn shutdown(&self, how: usize) -> Result<(), SocketError> {
        self.remote.shutdown(how)
    }

    pub(super) fn poll_state(&self) -> SocketPollState {
        self.remote.poll_state()
    }

    /// 状态在 netstack 中：无等待的投影不可得，epoll 把它作为候选并在任务上下文中精确复查。
    pub(super) fn try_poll_state(&self) -> Option<SocketPollState> {
        None
    }

    pub(super) fn readiness_generation(&self) -> u64 {
        self.notify_read
            .pipe()
            .readiness_generation(PipeDirection::Read)
    }

    pub(super) fn wait_sources(&self) -> super::SocketWaitSources {
        [
            Some(SocketWaitSource::notification(self.notify_read.pipe())),
            None,
            None,
        ]
    }

    /// 排空已观察的通知，使下一次等待不挂到陈旧的唤醒上；电平状态由随后的复查决定。
    pub(super) fn consume_wait_notifications(&self) {
        self.notify_read.drain_readiness();
    }

    pub(super) fn set_header_included(&self, enabled: bool) -> Result<(), SocketError> {
        self.remote
            .set_option(InetOptionChange::HeaderIncluded(enabled))
    }

    pub(super) fn set_packet_info(&self, enabled: bool) -> Result<(), SocketError> {
        self.remote
            .set_option(InetOptionChange::PacketInfo(enabled))
    }

    pub(super) fn set_reuse_address(&self, enabled: bool) -> Result<(), SocketError> {
        self.remote
            .set_option(InetOptionChange::ReuseAddress(enabled))
    }

    pub(super) fn set_broadcast(&self, enabled: bool) -> Result<(), SocketError> {
        self.remote.set_option(InetOptionChange::Broadcast(enabled))
    }

    /// 设置 socket 的路由标记。没有所在网络域的 NET_RAW 或 NET_ADMIN 权限时返回 EPERM。
    pub(super) fn set_mark(
        &self,
        mark: u32,
        authority: &crate::namespace::NamespaceAuthority,
    ) -> Result<(), SocketError> {
        use linux_abi::capability::Capability;

        let user = self.namespace.user_namespace();
        if !authority.capable(&user, Capability::NET_RAW)
            && !authority.capable(&user, Capability::NET_ADMIN)
        {
            return Err(SocketError::OperationNotPermitted);
        }
        self.remote.set_option(InetOptionChange::Mark(mark))
    }

    /// 已绑定后改绑或解绑需要所在域 owning user namespace 中的 `CAP_NET_RAW`（由 netstack 按绑定状态裁决）。
    pub(super) fn bind_to_device(
        &self,
        name: &[u8],
        authority: &crate::namespace::NamespaceAuthority,
    ) -> Result<(), SocketError> {
        let privileged = authority.capable(
            &self.namespace.user_namespace(),
            linux_abi::capability::Capability::NET_RAW,
        );
        self.remote.bind_to_device(name, privileged)
    }

    pub(super) fn set_hop_limit(&self, value: Option<u8>) -> Result<(), SocketError> {
        self.remote.set_option(InetOptionChange::HopLimit(value))
    }

    pub(super) fn set_no_delay(&self, enabled: bool) -> Result<(), SocketError> {
        self.remote.set_option(InetOptionChange::NoDelay(enabled))
    }

    pub(super) fn set_timer_option(
        &self,
        option: InternetOption,
        value: i32,
    ) -> Result<(), SocketError> {
        self.remote
            .set_option(InetOptionChange::Timer(option, value))
    }

    pub(super) fn icmp_filter(
        &self,
        update: Option<network_protocol::IcmpFilterUpdate>,
    ) -> Result<[u32; 8], SocketError> {
        self.remote.icmp_filter(update)
    }

    pub(super) fn set_ipv6_option(
        &self,
        option: InternetOption,
        value: i32,
    ) -> Result<(), SocketError> {
        self.remote.set_option(match option {
            InternetOption::Ipv6MulticastAll => InetOptionChange::Ipv6MulticastAll(value != 0),
            InternetOption::Ipv6MulticastInterface => {
                InetOptionChange::Ipv6MulticastInterface(value as u32)
            }
            InternetOption::Ipv6MulticastHopLimit if (-1..=255).contains(&value) => {
                InetOptionChange::Ipv6MulticastHopLimit(u8::try_from(value).ok())
            }
            InternetOption::Ipv6MulticastLoop if matches!(value, 0 | 1) => {
                InetOptionChange::Ipv6MulticastLoop(value != 0)
            }
            InternetOption::Ipv6Only => InetOptionChange::Ipv6Only(value != 0),
            InternetOption::Ipv6PacketInfo => InetOptionChange::Ipv6PacketInfo(value != 0),
            InternetOption::Ipv6HopLimit => {
                InetOptionChange::Ipv6HopLimit(u8::try_from(value).ok())
            }
            InternetOption::Ipv6Mtu => InetOptionChange::Ipv6Mtu(value),
            InternetOption::Ipv6DontFragment => InetOptionChange::Ipv6DontFragment(value != 0),
            InternetOption::Ipv6Checksum => InetOptionChange::Ipv6Checksum(value),
            _ => return Err(SocketError::Invalid),
        })
    }

    pub(super) fn ipv6_membership(
        &self,
        group: core::net::Ipv6Addr,
        interface: u32,
        join: bool,
    ) -> Result<(), SocketError> {
        self.remote.set_option(InetOptionChange::Ipv6Membership {
            group,
            interface,
            join,
        })
    }
    pub(super) fn ipv4_membership(
        &self,
        group: core::net::Ipv4Addr,
        interface: u32,
        address: core::net::Ipv4Addr,
        join: bool,
    ) -> Result<(), SocketError> {
        self.remote
            .set_option(InetOptionChange::Ipv4Membership {
                group,
                interface,
                address,
                join,
            })
            .map_err(|error| {
                if error == SocketError::NoMemory {
                    SocketError::NoBuffer
                } else {
                    error
                }
            })
    }
    pub(super) fn set_ipv4_multicast_interface(
        &self,
        interface: u32,
        address: core::net::Ipv4Addr,
    ) -> Result<(), SocketError> {
        self.remote
            .set_option(InetOptionChange::Ipv4MulticastInterface { interface, address })
    }
    pub(super) fn set_ipv4_multicast_option(
        &self,
        option: InternetOption,
        value: i32,
    ) -> Result<(), SocketError> {
        self.remote.set_option(match option {
            InternetOption::Ipv4MulticastHopLimit if (-1..=255).contains(&value) => {
                InetOptionChange::Ipv4MulticastHopLimit(u8::try_from(value).ok())
            }
            InternetOption::Ipv4MulticastLoop => InetOptionChange::Ipv4MulticastLoop(value != 0),
            _ => return Err(SocketError::Invalid),
        })
    }
    pub(super) fn set_ipv4_mtu_discovery(&self, mode: u8) -> Result<(), SocketError> {
        self.remote
            .set_option(InetOptionChange::Ipv4MtuDiscovery(mode))
    }
    pub(super) fn set_ipv4_multicast_all(&self, enabled: bool) -> Result<(), SocketError> {
        self.remote
            .set_option(InetOptionChange::Ipv4MulticastAll(enabled))
    }

    pub(super) fn set_max_segment(&self, value: i32) -> Result<(), SocketError> {
        self.remote.set_option(InetOptionChange::MaxSegment(value))
    }
    pub(super) fn set_md5_key(
        &self,
        selector: network_protocol::TcpMd5Selector,
        key: &[u8],
    ) -> Result<(), SocketError> {
        self.remote.set_md5_key(selector, key)
    }

    pub(super) fn option(&self, option: InternetOption) -> Result<i32, SocketError> {
        self.remote.option(option)
    }
}

impl Drop for InetSocket {
    /// 未发布（构造失败）的 socket 也摘除就绪登记；摘除是幂等的。
    fn drop(&mut self) {
        netstack::retire_readiness(&self.readiness);
    }
}

pub(crate) fn interface_index(scope: &NetNamespace, name: &[u8]) -> Result<u32, SocketError> {
    netstack::protocol(scope)?.interface_index(name)
}

pub(crate) fn interface_snapshot(
    scope: &NetNamespace,
    index: u32,
) -> Result<InterfaceSnapshot, SocketError> {
    netstack::protocol(scope)?.interface_snapshot(index)
}

pub(crate) fn interface_snapshots(
    scope: &NetNamespace,
) -> Result<Vec<InterfaceSnapshot>, SocketError> {
    netstack::protocol(scope)?.interface_snapshots()
}

pub(crate) fn network_snapshot(scope: &NetNamespace) -> Result<Vec<NetworkSnapshot>, SocketError> {
    netstack::protocol(scope)?.network_snapshot()
}

/// 冻结指定 NET 的实际地址；index 0 表示全部接口，未知非零接口返回 NoDevice。
pub(crate) fn address_snapshot(
    scope: &NetNamespace,
    index: u32,
) -> Result<network_protocol::AddressSnapshot, SocketError> {
    netstack::protocol(scope)?.address_snapshot(index)
}

/// 冻结指定 NET 的实际 IPv6 邻居；index 0 表示全部接口，未知非零接口返回 NoDevice。
pub(crate) fn neighbor_snapshot(
    scope: &NetNamespace,
    index: u32,
) -> Result<network_protocol::NeighborSnapshot, SocketError> {
    netstack::protocol(scope)?.neighbor_snapshot(index)
}

/// 查本机流的实际发送出口，由对应 NET 域完成选路和选源。
pub(crate) fn route_lookup(
    scope: &NetNamespace,
    query: network_protocol::routes::RouteLookup,
) -> Result<network_protocol::routes::RoutePath, SocketError> {
    netstack::protocol(scope)?.route_lookup(query)
}

pub(super) fn rule_snapshot(
    scope: &NetNamespace,
    family: Option<network_protocol::InetFamily>,
) -> Result<network_protocol::routes::RuleSnapshot, SocketError> {
    netstack::protocol(scope)?.rule_snapshot(family)
}

pub(crate) fn route_snapshot<A: network_protocol::routes::RouteAddress>(
    scope: &NetNamespace,
) -> Result<RoutingSnapshot<A>, SocketError> {
    netstack::protocol(scope)?.route_snapshot()
}

/// 同一 NET 发布代际的两族冻结路由，不用两个独立请求拼接 AF_UNSPEC。
/// # Errors
/// 域、传输、格式或资源失败，不返回部分集合。
pub(crate) fn route_snapshots(
    scope: &NetNamespace,
) -> Result<
    (
        RoutingSnapshot<core::net::Ipv4Addr>,
        RoutingSnapshot<core::net::Ipv6Addr>,
    ),
    SocketError,
> {
    netstack::protocol(scope)?.route_snapshots()
}

/// SIOCSIFMTU / IFLA_MTU。
pub(crate) fn configure_mtu(scope: &NetNamespace, index: u32, mtu: i32) -> Result<(), SocketError> {
    netstack::protocol(scope)?.configure_mtu(index, mtu)
}

/// SIOCSIFADDR / RTM_NEWADDR；`0.0.0.0` 删除地址。
pub(crate) fn configure_address(
    scope: &NetNamespace,
    index: u32,
    address: Ipv4Addr,
) -> Result<(), SocketError> {
    netstack::protocol(scope)?.configure_address(index, address)
}

pub(crate) fn configure_netmask(
    scope: &NetNamespace,
    index: u32,
    netmask: Ipv4Addr,
) -> Result<(), SocketError> {
    netstack::protocol(scope)?.configure_netmask(index, netmask)
}

pub(crate) fn configure_broadcast(
    scope: &NetNamespace,
    index: u32,
    broadcast: Ipv4Addr,
) -> Result<(), SocketError> {
    netstack::protocol(scope)?.configure_broadcast(index, broadcast)
}

pub(crate) fn configure_up(scope: &NetNamespace, index: u32, up: bool) -> Result<(), SocketError> {
    netstack::protocol(scope)?.configure_up(index, up)
}

/// SIOCADDRT/SIOCDELRT 与 RTM_NEWROUTE/RTM_DELROUTE 进入同一实际 FIB。
pub(crate) fn configure_route(
    scope: &NetNamespace,
    request: Ipv4RouteRequest,
    change: RouteChange,
) -> Result<(), Ipv4RouteError> {
    scope
        .protocol()
        .ok_or(Ipv4RouteError::NetworkUnreachable)?
        .configure_route(request, change)
}
