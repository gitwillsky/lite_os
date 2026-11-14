use super::{InetEndpoint, InetSocket, SocketError};
use network_protocol::InternetOption;

/// NetworkStack endpoint owner 保存的标准 SOL_SOCKET policy。
#[derive(Clone, Copy)]
pub(super) struct InetSocketOptions {
    /// 控制地址冲突是否允许两个 endpoint 共同 opt-in；缺失会让 daemon restart 错误 EADDRINUSE。
    pub(super) reuse_address: bool,
    /// 授权 UDP broadcast；缺失会让 DHCP 等标准 broadcast send 绕过 EACCES policy。
    pub(super) broadcast: bool,
    /// 缺省为零。新发送使用此标记，已提交的数据报保留提交时的值。
    pub(super) mark: u32,
    /// 实际NET设备index的native bound policy；名称由同一实际interface owner解析。
    /// None为未绑定，不能把它缩成bool/eth0常量而绕过多NIC的ingress与route约束。
    pub(super) bound_to_device: Option<u32>,
    /// TCP_NODELAY 关闭 Nagle；缺失会让 TLS/interactive stream 的标准 latency policy 被虚假接受。
    pub(super) no_delay: bool,
    /// native user_mss；实际握手广告/effective MSS 只属于 transport，不能用此 policy 冒充协商值。
    pub(super) user_mss: Option<u16>,
    /// TCP 原生 uc_ttl policy；None 使用默认 hop limit，并随实际 passive child 快照移交。
    /// 新建/补位/断开后重建 handles 都应用该 policy，否则首次 SYN 与后续数据会使用不同 TTL。
    pub(super) hop_limit: Option<u8>,
    /// IPv4 PMTU 发现模式；默认 WANT(1)，只由 endpoint owner 保存并在数据报发布时冻结。
    pub(super) ipv4_mtu_discovery: u8,
    /// IP_PKTINFO 的唯一 native socket flag；UDP/raw 接收返回真实 IP destination 供 ABI 编码。
    pub(super) packet_info: bool,
    /// IPv6 socket 的双栈准入；仅在尚未绑定时可改，避免改变既有端口占用空间。
    pub(super) ipv6_only: bool,
    /// IPV6_RECVPKTINFO 独立于 IP_PKTINFO；接收时由实际 policy 选择需要的投影，不经额外查询 RPC。
    pub(super) ipv6_packet_info: bool,
    /// Linux IPV6_MULTICAST_ALL：允许接收接口上其他成员登记的组；关闭后只接受自身成员。
    pub(super) ipv6_multicast_all: bool,
    /// IP_MULTICAST_ALL 的唯一策略；只能放宽已通过接口准入的包，不建立隐式成员。
    pub(super) ipv4_multicast_all: bool,
    /// IPv4 组播发送的实际接口；只用于未指定单次接口/设备绑定的发送。
    pub(super) ipv4_multicast_interface: Option<u32>,
    /// IPv4 组播源地址选择意图，也是标准 getter 的结果；零表示自动选源。
    pub(super) ipv4_multicast_address: core::net::Ipv4Addr,
    /// IPv4 组播 TTL，默认 1；不覆盖单播或已发布的报文。
    pub(super) ipv4_multicast_hop_limit: u8,
    /// IPv4 组播本机回送；在入队时冻结，否则重试会改变旧包的交付语义。
    pub(super) ipv4_multicast_loop: bool,
    /// 组播默认发送接口，仅在 scope/单次控制项/设备绑定均未选择接口时使用。
    pub(super) ipv6_multicast_interface: Option<u32>,
    /// 组播独立跳数，None 使用 1；发送时冻结，不能被后续单播策略覆盖。
    pub(super) ipv6_multicast_hop_limit: Option<u8>,
    /// 组播本机回送策略，在数据报发布时冻结，避免背压期间改选项重写旧包。
    pub(super) ipv6_multicast_loop: bool,
    /// 与 IPv4 TTL 独立的 IPv6 单播跳数；0 有效，不重写已排队数据报。
    pub(super) ipv6_hop_limit: Option<u8>,
    /// 应用显式的 IPv6 发送上限；只约束新发送，学习的 PMTU 仍由 IP 层唯一保存。
    pub(super) ipv6_mtu: Option<u32>,
    pub(super) ipv6_dont_fragment: bool,
    // OWNER: 唯一 IP_HDRINCL codec policy；RAW255 默认启用，改变时不重写已冻结的队列。
    pub(super) header_included: bool,
    /// 原生keepalive的唯一配置policy；实际deadline与probe count只在transport持有。
    pub(super) keep_alive: bool,
    pub(super) keep_idle: u16,
    pub(super) keep_interval: u16,
    pub(super) keep_count: u8,
    /// 原生主动握手预算及未确认数据期限；实际计数/起点不属于policy。
    pub(super) syn_retries: u8,
    pub(super) user_timeout: u32,
    /// native孤儿FIN_WAIT2期限；0选择系统默认，-1主动reset，仅在实际最后fput后生效。
    pub(super) linger2: i32,
}

impl Default for InetSocketOptions {
    fn default() -> Self {
        Self {
            reuse_address: false,
            broadcast: false,
            mark: 0,
            bound_to_device: None,
            no_delay: false,
            user_mss: None,
            hop_limit: None,
            ipv4_mtu_discovery: 1,
            packet_info: false,
            ipv6_only: false,
            ipv6_packet_info: false,
            ipv6_multicast_all: true,
            ipv4_multicast_all: true,
            ipv4_multicast_interface: None,
            ipv4_multicast_address: core::net::Ipv4Addr::UNSPECIFIED,
            ipv4_multicast_hop_limit: 1,
            ipv4_multicast_loop: true,
            ipv6_multicast_interface: None,
            ipv6_multicast_hop_limit: None,
            ipv6_multicast_loop: true,
            ipv6_hop_limit: None,
            ipv6_mtu: None,
            ipv6_dont_fragment: false,
            header_included: false,
            keep_alive: false,
            keep_idle: 7200,
            keep_interval: 75,
            keep_count: 9,
            syn_retries: 6,
            user_timeout: 0,
            linger2: 0,
        }
    }
}

impl InetSocketOptions {
    pub(super) fn multicast_all(&self, destination: core::net::IpAddr) -> bool {
        if destination.is_ipv4() {
            self.ipv4_multicast_all
        } else {
            self.ipv6_multicast_all
        }
    }
    /// 实际 wire 族决定发送策略；mapped IPv4 仍使用 IPv4 组播 policy。
    pub(super) fn multicast_loop(&self, destination: core::net::IpAddr) -> bool {
        destination.is_multicast()
            && if destination.is_ipv4() {
                self.ipv4_multicast_loop
            } else {
                self.ipv6_multicast_loop
            }
    }
    pub(super) fn datagram_hop_limit(
        &self,
        destination: core::net::IpAddr,
        default_unicast: u8,
    ) -> u8 {
        match (destination.is_ipv4(), destination.is_multicast()) {
            (true, true) => self.ipv4_multicast_hop_limit,
            (false, true) => self.ipv6_multicast_hop_limit.unwrap_or(1),
            (true, false) => self.hop_limit.unwrap_or(default_unicast),
            (false, false) => self.ipv6_hop_limit.unwrap_or(default_unicast),
        }
    }
    /// 物理目的事实始终在入队时冻结；投影请求在实际 receive/PEEK 时选定，不另存 endpoint policy。
    pub(super) fn receive_packet_info(
        &self,
        info: Option<network_protocol::InternetPacketInfo>,
    ) -> Option<network_protocol::InternetPacketInfo> {
        use network_protocol::InternetPacketInfo;
        match info? {
            InternetPacketInfo::Ipv4 {
                interface_index,
                specific_address,
                destination,
            }
            | InternetPacketInfo::DualStack {
                interface_index,
                specific_address,
                destination,
            } => match (self.packet_info, self.ipv6_packet_info) {
                (true, true) => Some(InternetPacketInfo::DualStack {
                    interface_index,
                    specific_address,
                    destination,
                }),
                (true, false) => Some(InternetPacketInfo::Ipv4 {
                    interface_index,
                    specific_address,
                    destination,
                }),
                (false, true) => Some(InternetPacketInfo::Ipv6 {
                    interface_index,
                    destination: destination.to_ipv6_mapped(),
                }),
                (false, false) => None,
            },
            info @ InternetPacketInfo::Ipv6 { .. } => self.ipv6_packet_info.then_some(info),
        }
    }
    /// 一个实际 TCP handle 的 mechanism 初始化；无分配，所有生命周期入口共用。
    pub(super) fn apply_tcp(&self, socket: &mut ::tcp::Socket<'_>) {
        let scoped=socket.remote_endpoint().into_iter().chain(socket.local_endpoint()).any(|endpoint|matches!(endpoint.addr,core::net::IpAddr::V6(address) if address.is_unicast_link_local()));
        // 链路范围属于本次真实连接；普通选项更新不能抹去被动握手从入口取得的 zone。
        socket.set_bound_interface(
            self.bound_to_device
                .or_else(|| scoped.then(|| socket.bound_interface()).flatten()),
        );
        socket.set_nagle_enabled(!self.no_delay);
        socket.set_hop_limit(
            if socket
                .remote_endpoint()
                .is_some_and(|peer| peer.addr.is_ipv6())
            {
                self.ipv6_hop_limit
            } else {
                self.hop_limit
            },
        );
        socket.set_user_mss(self.user_mss);
        socket.set_syn_retries(self.syn_retries);
        socket.set_linger2(self.linger2);
        socket.set_user_timeout((self.user_timeout != 0).then_some(
            ::tcp::time::Duration::from_millis(u64::from(self.user_timeout)),
        ));
        socket.set_keep_alive_policy(
            self.keep_alive
                .then_some(::tcp::time::Duration::from_secs(u64::from(self.keep_idle))),
            ::tcp::time::Duration::from_secs(u64::from(self.keep_interval)),
            self.keep_count,
        );
    }
}

impl super::NetworkStack {
    /// 标准 option mutation 只取得当前 endpoint 的唯一 policy，不查询 Task 或复制状态。
    fn options_mut(
        &mut self,
        endpoint: InetEndpoint,
    ) -> Result<&mut InetSocketOptions, SocketError> {
        Ok(match endpoint {
            InetEndpoint::Udp(handle) => {
                &mut self
                    .udp_endpoints
                    .get_mut(&handle)
                    .ok_or(SocketError::NotConnected)?
                    .options
            }
            InetEndpoint::Tcp(id) => {
                &mut self
                    .tcp_endpoints
                    .get_mut(&id)
                    .ok_or(SocketError::NotConnected)?
                    .options
            }
            InetEndpoint::Raw(handle) => {
                &mut self
                    .raw_endpoints
                    .get_mut(&handle)
                    .ok_or(SocketError::NotConnected)?
                    .options
            }
        })
    }
}

impl InetSocket {
    /// 修改 IPv4 的接口全组接收策略，不改变既有成员。
    /// # Parameters
    /// `enabled` 决定是否接收接口上其他 socket 已加入的组。
    /// # Returns
    /// 实际 endpoint 策略更新后返回 unit。
    /// # Errors
    /// Endpoint 关闭、网络域退出或 NET owner 不可用。
    pub(crate) fn set_ipv4_multicast_all(&self, enabled: bool) -> Result<(), SocketError> {
        let _operation = self.open_operation()?;
        self.stack()?
            .lock()?
            .options_mut(self.endpoint)?
            .ipv4_multicast_all = enabled;
        Ok(())
    }
    /// 设置 RAW 的实际 IP_HDRINCL 输入解释策略；非 RAW 返回 OperationNotSupported。
    pub(crate) fn set_header_included(&self, enabled: bool) -> Result<(), SocketError> {
        let _operation = self.open_operation()?;
        if !matches!(self.endpoint, InetEndpoint::Raw(_)) {
            return Err(SocketError::OperationNotSupported);
        }
        self.stack()?
            .lock()?
            .options_mut(self.endpoint)?
            .header_included = enabled;
        Ok(())
    }

    /// 设置 Linux `SO_REUSEADDR` 并由 endpoint owner 参与 bind collision policy。
    ///
    /// # Parameters
    ///
    /// - `enabled`: 非零 option value 的布尔投影。
    ///
    /// # Returns
    ///
    /// endpoint 存在时返回 unit。
    ///
    /// # Errors
    ///
    /// endpoint 已被删除时返回 NotConnected。
    pub(crate) fn set_reuse_address(&self, enabled: bool) -> Result<(), SocketError> {
        let _operation = self.open_operation()?;
        let mut network = self.stack()?.lock()?;
        match self.endpoint {
            InetEndpoint::Udp(handle) => {
                let super::NetworkStack {
                    udp_endpoints,
                    udp_ports,
                    ..
                } = &mut *network;
                let state = udp_endpoints
                    .get_mut(&handle)
                    .ok_or(SocketError::NotConnected)?;
                if let Some(lease) = state.port_lease {
                    state.port_lease = Some(udp_ports.set_reuse(lease, enabled));
                }
                state.options.reuse_address = enabled;
            }
            InetEndpoint::Tcp(id) => {
                let super::NetworkStack {
                    tcp_endpoints,
                    tcp_ports,
                    ..
                } = &mut *network;
                let state = tcp_endpoints
                    .get_mut(&id)
                    .ok_or(SocketError::NotConnected)?;
                if let Some(lease) = state.port_lease {
                    state.port_lease = Some(tcp_ports.set_reuse(lease, enabled));
                }
                state.options.reuse_address = enabled;
            }
            InetEndpoint::Raw(handle) => {
                network
                    .raw_endpoints
                    .get_mut(&handle)
                    .ok_or(SocketError::NotConnected)?
                    .options
                    .reuse_address = enabled;
            }
        }
        Ok(())
    }

    /// 设置 UDP limited/subnet broadcast 发送授权。
    ///
    /// # Parameters
    ///
    /// - `enabled`: 非零 option value 的布尔投影。
    ///
    /// # Returns
    ///
    /// UDP endpoint 存在时返回 unit。
    ///
    /// # Errors
    ///
    /// TCP 返回 OperationNotSupported；endpoint 消失返回 NotConnected。
    pub(crate) fn set_broadcast(&self, enabled: bool) -> Result<(), SocketError> {
        let _operation = self.open_operation()?;
        let mut network = self.stack()?.lock()?;
        network.options_mut(self.endpoint)?.broadcast = enabled;
        Ok(())
    }

    /// 更新新发送使用的标记。端点已关闭时返回 socket 生命周期错误。
    pub(crate) fn set_mark(&self, mark: u32) -> Result<(), SocketError> {
        let _operation = self.open_operation()?;
        let mut network = self.stack()?.lock()?;
        let options = network.options_mut(self.endpoint)?;
        let changed = options.mark != mark;
        options.mark = mark;
        drop(network);
        if changed && matches!(self.endpoint, InetEndpoint::Tcp(_)) {
            crate::worker::request_poll(self.namespace.id());
        }
        Ok(())
    }

    /// 将 endpoint 绑定到当前NET实际interface，空名称解除绑定。
    ///
    /// # Parameters
    ///
    /// - `name`: NUL 已剥离的 interface name bytes。
    /// - `privileged`: 调用者在域的 owning user namespace 中具有 `CAP_NET_RAW`。
    ///
    /// # Returns
    ///
    /// binding 状态提交给 endpoint owner 后返回 unit。
    ///
    /// # Errors
    ///
    /// 未知 interface 返回 NoDevice；endpoint 消失返回 NotConnected；新端口索引 OOM 返回 NoMemory，保留旧绑定。
    pub(crate) fn bind_to_device(&self, name: &[u8], privileged: bool) -> Result<(), SocketError> {
        let _operation = self.open_operation()?;
        let mut network = self.stack()?.lock()?;
        let index = if name.is_empty() {
            None
        } else {
            Some(network.interface_index(name)?)
        };
        let options = network.options_mut(self.endpoint)?;
        // Linux sock_bindtoindex_locked只对已经绑定的socket要求owning NET UserNS的NET_RAW。
        // 未绑定的首次绑定允许普通actor；已绑定时同名/解绑也重新授权，名字错误在授权之前。
        if options.bound_to_device.is_some() && !privileged {
            return Err(SocketError::OperationNotPermitted);
        }
        match self.endpoint {
            InetEndpoint::Udp(handle) => super::udp::bind_to_device(&mut network, handle, index)?,
            InetEndpoint::Tcp(id) => super::tcp::bind_to_device(&mut network, id, index)?,
            InetEndpoint::Raw(_) => {
                network.options_mut(self.endpoint)?.bound_to_device = index;
            }
        }
        drop(network);
        crate::worker::request_poll(self.namespace.id());
        Ok(())
    }

    /// 原生 IP_TTL 在 TCP/UDP/raw 的实际传输或 policy owner 更新；None 恢复默认。
    pub(crate) fn set_hop_limit(&self, value: Option<u8>) -> Result<(), SocketError> {
        let _operation = self.open_operation()?;
        if value == Some(0) {
            return Err(SocketError::Invalid);
        }
        if matches!(self.endpoint, InetEndpoint::Tcp(_)) {
            return super::tcp::set_hop_limit(self, value);
        }
        let mut network = self.stack()?.lock()?;
        let super::NetworkStack {
            udp_endpoints,
            raw_endpoints,
            ..
        } = &mut *network;
        match self.endpoint {
            InetEndpoint::Udp(handle) => {
                udp_endpoints
                    .get_mut(&handle)
                    .ok_or(SocketError::NotConnected)?
                    .options
                    .hop_limit = value;
            }
            InetEndpoint::Tcp(_) => unreachable!("TCP option mutation belongs to TCP owner"),
            InetEndpoint::Raw(handle) => {
                raw_endpoints
                    .get_mut(&handle)
                    .ok_or(SocketError::NotConnected)?
                    .options
                    .hop_limit = value;
            }
        }
        Ok(())
    }

    /// 原子更新 IPv4 组播默认发送接口与地址选择意图。
    /// # Parameters
    /// `interface` 非零时按实际 ifindex 选择，否则按 `address` 查找；均为零时恢复自动选择。
    /// # Returns
    /// NET owner 完整发布两个选择字段后返回 unit，既有成员和队列不变。
    /// # Errors
    /// TCP/绑定冲突返回 Invalid，接口或地址不存在返回 AddressNotAvailable；失败保持旧策略。
    pub(crate) fn set_ipv4_multicast_interface(
        &self,
        interface: u32,
        address: core::net::Ipv4Addr,
    ) -> Result<(), SocketError> {
        let _operation = self.open_operation()?;
        if matches!(self.endpoint, InetEndpoint::Tcp(_)) {
            return Err(SocketError::Invalid);
        }
        let mut network = self.stack()?.lock()?;
        let selected = if interface != 0 {
            network.interface(interface).ok().map(|entry| entry.index)
        } else if !address.is_unspecified() {
            network
                .interfaces
                .iter()
                .find(|entry| entry.state.address == Some(address))
                .map(|entry| entry.index)
        } else {
            None
        };
        if (interface != 0 || !address.is_unspecified()) && selected.is_none() {
            return Err(SocketError::AddressNotAvailable);
        }
        let options = network.options_mut(self.endpoint)?;
        if selected.is_some()
            && options.bound_to_device.is_some()
            && selected != options.bound_to_device
        {
            return Err(SocketError::Invalid);
        }
        options.ipv4_multicast_interface = selected;
        options.ipv4_multicast_address = address;
        Ok(())
    }
    /// 修改 IPv4 组播 TTL/回送，默认与限制遵循标准 IP 选项。
    /// # Parameters
    /// `option` 为 IPv4 组播 TTL 或回送；TTL 为 -1/0..255，回送为 0/1。
    /// # Returns
    /// 仅更新未来发送的策略。
    /// # Errors
    /// 非法选项/值、TCP 设置 TTL 返回 Invalid；失效 endpoint 返回领域错误。
    pub(crate) fn set_ipv4_multicast_option(
        &self,
        option: InternetOption,
        value: i32,
    ) -> Result<(), SocketError> {
        let _operation = self.open_operation()?;
        let mut network = self.stack()?.lock()?;
        if matches!(self.endpoint, InetEndpoint::Tcp(_))
            && option == InternetOption::Ipv4MulticastHopLimit
        {
            return Err(SocketError::Invalid);
        }
        let options = network.options_mut(self.endpoint)?;
        match option {
            InternetOption::Ipv4MulticastHopLimit if (-1..=255).contains(&value) => {
                options.ipv4_multicast_hop_limit = u8::try_from(value).unwrap_or(1)
            }
            InternetOption::Ipv4MulticastLoop if matches!(value, 0 | 1) => {
                options.ipv4_multicast_loop = value != 0
            }
            _ => return Err(SocketError::Invalid),
        }
        Ok(())
    }

    /// 修改实际 IPv6 组播发送策略；只有 loop 对 TCP 可设，IF/hops 只属于 datagram。
    /// # Parameters
    /// `option` 为组播 IF/hops/loop；`value` 是接口、-1/0..255 跳数或 0/1 布尔。
    /// # Returns
    /// 原 owner 策略更新后返回 unit，已排队数据报不变。
    /// # Errors
    /// 类型/地址族不支持、未知接口、设备绑定冲突或非法值，失败保持旧策略。
    fn set_multicast_option(
        &self,
        option: network_protocol::InternetOption,
        value: i32,
    ) -> Result<(), SocketError> {
        use network_protocol::InternetOption;
        let _operation = self.open_operation()?;
        let mut network = self.stack()?.lock()?;
        let ipv6 = match self.endpoint {
            InetEndpoint::Udp(id) => network.udp_endpoints[&id].is_ipv6(),
            InetEndpoint::Raw(id) => network.raw_endpoints[&id].is_ipv6(),
            InetEndpoint::Tcp(id) => network.tcp_endpoints[&id].is_ipv6(),
        };
        if !ipv6
            || matches!(self.endpoint, InetEndpoint::Tcp(_))
                && option != InternetOption::Ipv6MulticastLoop
        {
            return Err(SocketError::OperationNotSupported);
        }
        if option == InternetOption::Ipv6MulticastInterface && value != 0 {
            network.interface(value as u32)?;
        }
        let options = network.options_mut(self.endpoint)?;
        match option {
            InternetOption::Ipv6MulticastInterface => {
                let index = (value != 0).then_some(value as u32);
                if index.is_some()
                    && options.bound_to_device.is_some()
                    && options.bound_to_device != index
                {
                    return Err(SocketError::Invalid);
                }
                options.ipv6_multicast_interface = index;
            }
            InternetOption::Ipv6MulticastHopLimit if (-1..=255).contains(&value) => {
                options.ipv6_multicast_hop_limit = u8::try_from(value).ok();
            }
            InternetOption::Ipv6MulticastLoop if matches!(value, 0 | 1) => {
                options.ipv6_multicast_loop = value != 0;
            }
            _ => return Err(SocketError::Invalid),
        }
        Ok(())
    }
    /// 修改实际 IPv6 socket policy；绑定后不可改变 V6ONLY，失败不改变原 policy。
    pub(crate) fn set_ipv6_option(
        &self,
        option: InternetOption,
        value: i32,
    ) -> Result<(), SocketError> {
        if matches!(
            option,
            InternetOption::Ipv6MulticastInterface
                | InternetOption::Ipv6MulticastHopLimit
                | InternetOption::Ipv6MulticastLoop
        ) {
            return self.set_multicast_option(option, value);
        }
        let _operation = self.open_operation()?;
        if matches!(self.endpoint, InetEndpoint::Tcp(_)) {
            return super::tcp::set_ipv6_option(self, option, value);
        }
        if let InetEndpoint::Raw(id) = self.endpoint {
            return super::raw::set_ipv6_option(self, id, option, value);
        }
        let InetEndpoint::Udp(id) = self.endpoint else {
            return Err(SocketError::OperationNotSupported);
        };
        let mut network = self.stack()?.lock()?;
        let state = network
            .udp_endpoints
            .get_mut(&id)
            .ok_or(SocketError::NotConnected)?;
        if !state.is_ipv6() {
            return Err(SocketError::OperationNotSupported);
        }
        match option {
            InternetOption::Ipv6MulticastAll => state.options.ipv6_multicast_all = value != 0,
            InternetOption::Ipv6PacketInfo => state.options.ipv6_packet_info = value != 0,
            InternetOption::Ipv6Only if !state.is_bound() => state.options.ipv6_only = value != 0,
            InternetOption::Ipv6Only => return Err(SocketError::Invalid),
            InternetOption::Ipv6HopLimit if (-1..=255).contains(&value) => {
                state.options.ipv6_hop_limit = u8::try_from(value).ok()
            }
            InternetOption::Ipv6DontFragment => state.options.ipv6_dont_fragment = value != 0,
            InternetOption::Ipv6Mtu if value == 0 || value >= 1280 => {
                state.options.ipv6_mtu = (value != 0).then_some(value as u32)
            }
            _ => return Err(SocketError::Invalid),
        }
        Ok(())
    }

    /// 从同一 operation/stack guard 查询真实值；不保存 socket façade 的 option 镜像。
    pub(crate) fn option(&self, option: InternetOption) -> Result<i32, SocketError> {
        let _operation = self.open_operation()?;
        let network = self.stack()?.lock()?;
        let error_option = match option {
            InternetOption::Ipv4ReceiveError => Some((false, false)),
            InternetOption::Ipv6ReceiveError => Some((true, false)),
            InternetOption::Ipv4ReceiveErrorExtensions => Some((false, true)),
            InternetOption::Ipv6ReceiveErrorExtensions => Some((true, true)),
            _ => None,
        };
        if let Some((ipv6, extensions)) = error_option {
            return network
                .receive_error_enabled(self.endpoint, ipv6, extensions)
                .map(i32::from);
        }
        if option == InternetOption::Protocol {
            return Ok(match self.endpoint {
                InetEndpoint::Udp(_) => 17,
                InetEndpoint::Tcp(_) => 6,
                InetEndpoint::Raw(id) => super::raw::protocol(&network, id)?,
            });
        }
        if option == InternetOption::Ipv6Checksum {
            return match self.endpoint {
                InetEndpoint::Raw(id) => super::raw::checksum(&network, id),
                _ => Err(SocketError::OperationNotSupported),
            };
        }
        if let InternetOption::MaxSegment = option {
            let InetEndpoint::Tcp(id) = self.endpoint else {
                return Err(SocketError::OperationNotSupported);
            };
            return super::tcp::max_segment_size(&network, id);
        }
        if matches!(option, InternetOption::Ipv4Mtu | InternetOption::Ipv6Mtu) {
            let ipv4 = option == InternetOption::Ipv4Mtu;
            return match self.endpoint {
                InetEndpoint::Udp(id) => super::udp::path_mtu(&network, id, ipv4),
                InetEndpoint::Tcp(id) => super::tcp::path_mtu(&network, id, ipv4),
                InetEndpoint::Raw(id) => super::raw::path_mtu(&network, id, ipv4),
            };
        }
        let options = match self.endpoint {
            InetEndpoint::Udp(handle) => {
                &network
                    .udp_endpoints
                    .get(&handle)
                    .ok_or(SocketError::NotConnected)?
                    .options
            }
            InetEndpoint::Tcp(id) => {
                &network
                    .tcp_endpoints
                    .get(&id)
                    .ok_or(SocketError::NotConnected)?
                    .options
            }
            InetEndpoint::Raw(handle) => {
                &network
                    .raw_endpoints
                    .get(&handle)
                    .ok_or(SocketError::NotConnected)?
                    .options
            }
        };
        if matches!(
            option,
            InternetOption::Ipv6Only
                | InternetOption::Ipv6MulticastAll
                | InternetOption::Ipv6MulticastInterface
                | InternetOption::Ipv6MulticastHopLimit
                | InternetOption::Ipv6MulticastLoop
                | InternetOption::Ipv6HopLimit
                | InternetOption::Ipv6PacketInfo
                | InternetOption::Ipv6DontFragment
        ) && !match self.endpoint {
            InetEndpoint::Udp(id) => network.udp_endpoints[&id].is_ipv6(),
            InetEndpoint::Tcp(id) => network.tcp_endpoints[&id].is_ipv6(),
            InetEndpoint::Raw(id) => network.raw_endpoints[&id].is_ipv6(),
        } {
            return Err(SocketError::OperationNotSupported);
        }
        Ok(match option {
            InternetOption::Protocol
            | InternetOption::Ipv6Checksum
            | InternetOption::Ipv4ReceiveError
            | InternetOption::Ipv6ReceiveError
            | InternetOption::Ipv4ReceiveErrorExtensions
            | InternetOption::Ipv6ReceiveErrorExtensions => {
                unreachable!("RAW/transport query is handled before common policy")
            }
            InternetOption::Ipv4Mtu | InternetOption::Ipv6Mtu => {
                unreachable!("path MTU belongs to IP route, not option policy")
            }
            InternetOption::Ipv6DontFragment => i32::from(options.ipv6_dont_fragment),
            InternetOption::Ipv6Only => i32::from(options.ipv6_only),
            InternetOption::Ipv6MulticastAll => i32::from(options.ipv6_multicast_all),
            InternetOption::Ipv4MtuDiscovery => i32::from(options.ipv4_mtu_discovery),
            InternetOption::Ipv4MulticastAll => i32::from(options.ipv4_multicast_all),
            InternetOption::Ipv4MulticastInterface => {
                u32::from(options.ipv4_multicast_address) as i32
            }
            InternetOption::Ipv4MulticastHopLimit => i32::from(options.ipv4_multicast_hop_limit),
            InternetOption::Ipv4MulticastLoop => i32::from(options.ipv4_multicast_loop),
            InternetOption::Ipv6MulticastInterface => {
                options.ipv6_multicast_interface.unwrap_or(0) as i32
            }
            InternetOption::Ipv6MulticastHopLimit => {
                i32::from(options.ipv6_multicast_hop_limit.unwrap_or(1))
            }
            InternetOption::Ipv6MulticastLoop => i32::from(options.ipv6_multicast_loop),
            InternetOption::Ipv6PacketInfo => i32::from(options.ipv6_packet_info),
            InternetOption::Ipv6HopLimit => {
                i32::from(options.ipv6_hop_limit.unwrap_or_else(|| {
                    let route = match self.endpoint {
                        InetEndpoint::Udp(id) => super::udp::connected_output(&network, id, false),
                        InetEndpoint::Tcp(id) => super::tcp::connected_output(&network, id, false),
                        InetEndpoint::Raw(id) => super::raw::connected_output(&network, id, false),
                    };
                    route.map_or(super::ipv6::DEFAULT_HOP_LIMIT, |route| route.hop_limit)
                }))
            }
            InternetOption::ReuseAddress => i32::from(options.reuse_address),
            InternetOption::Mark => options.mark as i32,
            InternetOption::Broadcast => i32::from(options.broadcast),
            InternetOption::BoundToDevice => options.bound_to_device.unwrap_or(0) as i32,
            InternetOption::HopLimit => {
                i32::from(options.hop_limit.unwrap_or(super::DEFAULT_IPV4_HOP_LIMIT))
            }
            InternetOption::NoDelay if matches!(self.endpoint, InetEndpoint::Tcp(_)) => {
                i32::from(options.no_delay)
            }
            InternetOption::NoDelay => return Err(SocketError::OperationNotSupported),
            InternetOption::HeaderIncluded if matches!(self.endpoint, InetEndpoint::Raw(_)) => {
                i32::from(options.header_included)
            }
            InternetOption::HeaderIncluded => return Err(SocketError::OperationNotSupported),
            InternetOption::PacketInfo => i32::from(options.packet_info),
            InternetOption::KeepAlive => i32::from(options.keep_alive),
            InternetOption::KeepIdle
            | InternetOption::KeepInterval
            | InternetOption::KeepCount
            | InternetOption::SynRetries
            | InternetOption::UserTimeout
            | InternetOption::Linger2 => {
                if !matches!(self.endpoint, InetEndpoint::Tcp(_)) {
                    return Err(SocketError::OperationNotSupported);
                }
                match option {
                    InternetOption::KeepIdle => i32::from(options.keep_idle),
                    InternetOption::KeepInterval => i32::from(options.keep_interval),
                    InternetOption::SynRetries => i32::from(options.syn_retries),
                    InternetOption::UserTimeout => options.user_timeout as i32,
                    InternetOption::Linger2 => {
                        if options.linger2 == 0 {
                            60
                        } else {
                            options.linger2
                        }
                    }
                    _ => i32::from(options.keep_count),
                }
            }
            InternetOption::MaxSegment => unreachable!("actual TCP query above"),
        })
    }

    /// 修改 IPv4 数据报的路径 MTU 策略；不是只改变 getter 的 façade 状态。
    /// # Parameters
    /// `mode` 为标准逻辑模式 0..5，旧队列沿发布时策略继续发送。
    /// # Returns
    /// 实际 UDP/RAW owner 更新后返回 unit。
    /// # Errors
    /// 非法值、TCP 尚未提供该控制或失效 endpoint；失败保留原策略。
    pub(crate) fn set_ipv4_mtu_discovery(&self, mode: u8) -> Result<(), SocketError> {
        let _operation = self.open_operation()?;
        if mode > 5 {
            return Err(SocketError::Invalid);
        }
        if matches!(self.endpoint, InetEndpoint::Tcp(_)) {
            return Err(SocketError::OperationNotSupported);
        }
        self.stack()?
            .lock()?
            .options_mut(self.endpoint)?
            .ipv4_mtu_discovery = mode;
        Ok(())
    }
    /// IP_PKTINFO flag 的标准 mutation；TCP 保留 native flag，UDP/raw 才输出逐包 destination。
    pub(crate) fn set_packet_info(&self, enabled: bool) -> Result<(), SocketError> {
        let _operation = self.open_operation()?;
        self.stack()?
            .lock()?
            .options_mut(self.endpoint)?
            .packet_info = enabled;
        Ok(())
    }

    pub(crate) fn set_no_delay(&self, enabled: bool) -> Result<(), SocketError> {
        let _operation = self.open_operation()?;
        super::tcp::set_no_delay(self, enabled)
    }

    /// native keepalive配置在operation/NET guard一次更新，未完成的child沿用parent当前policy。
    pub(crate) fn set_timer_option(
        &self,
        option: InternetOption,
        value: i32,
    ) -> Result<(), SocketError> {
        match option {
            InternetOption::KeepAlive => {}
            InternetOption::KeepIdle | InternetOption::KeepInterval
                if (1..=32767).contains(&value) => {}
            InternetOption::KeepCount | InternetOption::SynRetries
                if (1..=127).contains(&value) => {}
            InternetOption::UserTimeout if value >= 0 => {}
            InternetOption::Linger2 => {}
            _ => return Err(SocketError::Invalid),
        }
        let _operation = self.open_operation()?;
        if matches!(self.endpoint, InetEndpoint::Tcp(_)) {
            return super::tcp::set_timer_option(self, option, value);
        }
        if !matches!(option, InternetOption::KeepAlive) {
            return Err(SocketError::OperationNotSupported);
        }
        self.stack()?.lock()?.options_mut(self.endpoint)?.keep_alive = value != 0;
        Ok(())
    }

    /// 固定 Linux TCP_MIN_MSS/MAX_TCP_WINDOW；0 清除 next-open clamp，失败不改变 policy。
    pub(crate) fn set_max_segment(&self, value: i32) -> Result<(), SocketError> {
        if value != 0 && !(88..=32767).contains(&value) {
            return Err(SocketError::Invalid);
        }
        let _operation = self.open_operation()?;
        if !matches!(self.endpoint, InetEndpoint::Tcp(_)) {
            return Err(SocketError::OperationNotSupported);
        }
        super::tcp::set_max_segment(self, (value != 0).then_some(value as u16))
    }
}

/// 按 IPv4 ip_cmsg_send 顺序应用控制项；IPv6 socket 的 mapped 发送还消费 mapped IPV6_PKTINFO。
/// 零 IPv4 源清除前一次选择，零接口不清除出口；再按组播目的补默认接口和源意图，不改 socket 绑定。
impl super::NetworkStack {
    pub(super) fn ipv4_packet_info(
        &self,
        controls: &[network_protocol::InternetSendPacketInfo],
        ipv6: bool,
        source: Option<core::net::Ipv4Addr>,
        options: &InetSocketOptions,
        destination: core::net::Ipv4Addr,
    ) -> Result<(Option<core::net::Ipv4Addr>, Option<u32>), SocketError> {
        let mut outgoing = options.bound_to_device;
        use network_protocol::{InetFamily, InternetSendPacketInfo};
        // None 是没有源覆盖，Some(None) 是显式零源；仅验证本次覆盖，不重新授权已绑定且后来删去的地址。
        let mut requested = None;
        for control in controls {
            let (interface, address) = match *control {
                InternetSendPacketInfo::Ipv4 {
                    interface_index,
                    source_address,
                } => (interface_index, source_address),
                InternetSendPacketInfo::Ipv6 {
                    interface_index,
                    source_address,
                } if ipv6 => (
                    interface_index,
                    source_address
                        .to_ipv4_mapped()
                        .ok_or(SocketError::Invalid)?,
                ),
                InternetSendPacketInfo::Invalid(family) if family == InetFamily::Ipv4 || ipv6 => {
                    return Err(SocketError::Invalid);
                }
                _ => continue,
            };
            outgoing = interface.or(outgoing);
            requested = Some((!address.is_unspecified()).then_some(address));
        }
        if let Some(Some(source)) = requested
            && !self.has_local_address(source)
        {
            return Err(SocketError::NetworkUnreachable);
        }
        if let Some(index) = outgoing {
            self.interface(index)?;
        }
        let mut source = requested.unwrap_or(source);
        if destination.is_multicast() {
            outgoing = outgoing.or(options.ipv4_multicast_interface);
            if source.is_none() && !options.ipv4_multicast_address.is_unspecified() {
                let address = options.ipv4_multicast_address;
                if !self.has_local_address(address) {
                    return Err(SocketError::NetworkUnreachable);
                }
                source = Some(address);
            }
        }
        Ok((source, outgoing))
    }
}

impl super::NetworkStack {
    /// 双族 UDP/RAW 共用的 IPv6 packet-info 准入；控制按顺序裁决，零源不清除此前显式源。
    /// # Errors
    /// zone/设备/控制接口冲突、未知接口、非法源或缺少链路地址作用域；失败不修改绑定。
    pub(super) fn ipv6_packet_info(
        &self,
        controls: &[network_protocol::InternetSendPacketInfo],
        bound: core::net::Ipv6Addr,
        bound_interface: Option<u32>,
        multicast_interface: Option<u32>,
        peer: core::net::SocketAddrV6,
    ) -> Result<(core::net::IpAddr, Option<u32>), SocketError> {
        let mut outgoing = bound_interface;
        if (peer.ip().is_unicast_link_local()
            || peer.ip().is_multicast() && peer.ip().octets()[1] & 15 <= 2)
            && peer.scope_id() != 0
        {
            if outgoing.is_some_and(|old| old != peer.scope_id()) {
                return Err(SocketError::Invalid);
            }
            outgoing = Some(peer.scope_id());
        }
        let mut source = core::net::Ipv6Addr::UNSPECIFIED;
        for control in controls {
            let (index, address) = match *control {
                network_protocol::InternetSendPacketInfo::Ipv6 {
                    interface_index,
                    source_address,
                } => (interface_index, source_address),
                network_protocol::InternetSendPacketInfo::Invalid(
                    network_protocol::InetFamily::Ipv6,
                ) => return Err(SocketError::Invalid),
                _ => continue,
            };
            if let Some(index) = index {
                if outgoing.is_some_and(|old| old != index) {
                    return Err(SocketError::Invalid);
                }
                outgoing = Some(index);
            }
            if let Some(index) = outgoing {
                self.interface(index)?;
            }
            let candidate = outgoing.or_else(|| {
                if peer.ip().is_multicast() {
                    multicast_interface
                } else {
                    None
                }
            });
            if address.is_unicast_link_local() && candidate.is_none() {
                return Err(SocketError::Invalid);
            }
            if !address.is_unspecified() {
                if !self.has_local_ip(address.into(), candidate) {
                    return Err(SocketError::Invalid);
                }
                source = address;
            }
        }
        if peer.ip().is_multicast() && outgoing.is_none() {
            outgoing = multicast_interface;
        }
        Ok((
            if source.is_unspecified() {
                // 组播绑定只限制接收目的，不能把组地址作为发送源；仍由同一 IP owner 选源。
                if bound.is_multicast() {
                    core::net::Ipv6Addr::UNSPECIFIED
                } else {
                    bound
                }
            } else {
                source
            }
            .into(),
            outgoing,
        ))
    }
}
