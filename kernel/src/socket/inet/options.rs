use super::{InetEndpoint, InetSocket, SocketError};

/// 已实现的 INET 选项查询；实际值只读取 protocol owner，ABI number 留在 syscall。
#[derive(Clone, Copy)]
pub(crate) enum InternetOption {
    ReuseAddress,
    Broadcast,
    BoundToDevice,
    HopLimit,
    HeaderIncluded,
    NoDelay,
    MaxSegment,
    PacketInfo,
    KeepAlive,
    KeepIdle,
    KeepInterval,
    KeepCount,
    SynRetries,
    UserTimeout,
    Linger2,
}

/// NetworkStack endpoint owner 保存的标准 SOL_SOCKET policy。
#[derive(Clone, Copy)]
pub(super) struct InetSocketOptions {
    /// 控制地址冲突是否允许两个 endpoint 共同 opt-in；缺失会让 daemon restart 错误 EADDRINUSE。
    pub(super) reuse_address: bool,
    /// 授权 UDP broadcast；缺失会让 DHCP 等标准 broadcast send 绕过 EACCES policy。
    pub(super) broadcast: bool,
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
    /// IP_PKTINFO 的唯一 native socket flag；UDP/raw 接收返回真实 IP destination 供 ABI 编码。
    pub(super) packet_info: bool,
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
            bound_to_device: None,
            no_delay: false,
            user_mss: None,
            hop_limit: None,
            packet_info: false,
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
    /// 一个实际 TCP handle 的 mechanism 初始化；无分配，所有生命周期入口共用。
    pub(super) fn apply_tcp(&self, socket: &mut smoltcp::socket::tcp::Socket<'_>) {
        socket.set_bound_interface(self.bound_to_device);
        socket.set_nagle_enabled(!self.no_delay);
        socket.set_hop_limit(self.hop_limit);
        socket.set_user_mss(self.user_mss);
        socket.set_syn_retries(self.syn_retries);
        socket.set_linger2(self.linger2);
        socket.set_user_timeout((self.user_timeout != 0).then_some(
            smoltcp::time::Duration::from_millis(u64::from(self.user_timeout)),
        ));
        socket.set_keep_alive_policy(
            self.keep_alive
                .then_some(smoltcp::time::Duration::from_secs(u64::from(
                    self.keep_idle,
                ))),
            smoltcp::time::Duration::from_secs(u64::from(self.keep_interval)),
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
                    .endpoints
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
    /// 将超时设置与 passive handshake 快照串行化；失败不改变 policy。
    /// 所有 INET timeout setter 按 operation → stack → policy 的顺序进入唯一 owner。
    pub(in crate::socket) fn set_timeout(
        &self,
        direction: crate::socket::SocketIoDirection,
        value: crate::socket::SocketTimeout,
    ) -> Result<(), SocketError> {
        let _operation = self.open_operation()?;
        let _network = self.stack()?.lock()?;
        self.timeouts.set(direction, value);
        Ok(())
    }

    /// 设置 RAW 的实际 IP_HDRINCL 输入解释策略；非 RAW 返回 OperationNotSupported。
    pub(in crate::socket) fn set_header_included(&self, enabled: bool) -> Result<(), SocketError> {
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
    pub(in crate::socket) fn set_reuse_address(&self, enabled: bool) -> Result<(), SocketError> {
        let _operation = self.open_operation()?;
        let mut network = self.stack()?.lock()?;
        match self.endpoint {
            InetEndpoint::Udp(handle) => {
                let super::NetworkStack {
                    endpoints,
                    udp_ports,
                    ..
                } = &mut *network;
                let state = endpoints
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
    pub(in crate::socket) fn set_broadcast(&self, enabled: bool) -> Result<(), SocketError> {
        let _operation = self.open_operation()?;
        let mut network = self.stack()?.lock()?;
        network.options_mut(self.endpoint)?.broadcast = enabled;
        Ok(())
    }

    /// 将 endpoint 绑定到当前NET实际interface，空名称解除绑定。
    ///
    /// # Parameters
    ///
    /// - `name`: NUL 已剥离的 interface name bytes。
    ///
    /// # Returns
    ///
    /// binding 状态提交给 endpoint owner 后返回 unit。
    ///
    /// # Errors
    ///
    /// 未知 interface 返回 NoDevice；endpoint 消失返回 NotConnected。
    pub(in crate::socket) fn bind_to_device(
        &self,
        name: &[u8],
        authority: &crate::namespace::NamespaceAuthority,
    ) -> Result<(), SocketError> {
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
        if options.bound_to_device.is_some()
            && !authority.capable(
                &self.memory.namespace_ref().user_namespace(),
                syscall_abi::capability::Capability::NET_RAW,
            )
        {
            return Err(SocketError::OperationNotPermitted);
        }
        options.bound_to_device = index;
        match self.endpoint {
            InetEndpoint::Udp(handle) => network
                .sockets
                .get_mut::<smoltcp::socket::udp::Socket<'static>>(handle)
                .set_bound_interface(index),
            InetEndpoint::Tcp(id) => super::tcp::apply_bound_interface(&mut network, id, index)?,
            InetEndpoint::Raw(handle) => network
                .sockets
                .get_mut::<smoltcp::socket::raw::Socket<'static>>(handle)
                .set_bound_interface(index),
        }
        Ok(())
    }

    /// 原生 IP_TTL 在 TCP/UDP/raw 的实际传输或 policy owner 更新；None 恢复默认。
    pub(in crate::socket) fn set_hop_limit(&self, value: Option<u8>) -> Result<(), SocketError> {
        let _operation = self.open_operation()?;
        if matches!(self.endpoint, InetEndpoint::Tcp(_)) {
            return super::tcp::set_hop_limit(self, value);
        }
        let mut network = self.stack()?.lock()?;
        let super::NetworkStack {
            sockets,
            endpoints,
            raw_endpoints,
            ..
        } = &mut *network;
        match self.endpoint {
            InetEndpoint::Udp(handle) => {
                endpoints
                    .get_mut(&handle)
                    .ok_or(SocketError::NotConnected)?
                    .options
                    .hop_limit = value;
                sockets
                    .get_mut::<smoltcp::socket::udp::Socket<'static>>(handle)
                    .set_hop_limit(value);
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

    /// 从同一 operation/stack guard 查询真实值；不保存 socket façade 的 option 镜像。
    pub(in crate::socket) fn option(
        &self,
        option: crate::socket::InternetOption,
    ) -> Result<i32, SocketError> {
        use crate::socket::InternetOption;
        let _operation = self.open_operation()?;
        let network = self.stack()?.lock()?;
        if let InternetOption::MaxSegment = option {
            let InetEndpoint::Tcp(id) = self.endpoint else {
                return Err(SocketError::OperationNotSupported);
            };
            return super::tcp::max_segment_size(&network, id);
        }
        let options = match self.endpoint {
            InetEndpoint::Udp(handle) => {
                &network
                    .endpoints
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
        Ok(match option {
            InternetOption::ReuseAddress => i32::from(options.reuse_address),
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

    /// IP_PKTINFO flag 的标准 mutation；TCP 保留 native flag，UDP/raw 才输出逐包 destination。
    pub(in crate::socket) fn set_packet_info(&self, enabled: bool) -> Result<(), SocketError> {
        let _operation = self.open_operation()?;
        self.stack()?
            .lock()?
            .options_mut(self.endpoint)?
            .packet_info = enabled;
        Ok(())
    }

    pub(in crate::socket) fn set_no_delay(&self, enabled: bool) -> Result<(), SocketError> {
        let _operation = self.open_operation()?;
        super::tcp::set_no_delay(self, enabled)
    }

    /// native keepalive配置在operation/NET guard一次更新，未完成的child沿用parent当前policy。
    pub(in crate::socket) fn set_timer_option(
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
    pub(in crate::socket) fn set_max_segment(&self, value: i32) -> Result<(), SocketError> {
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
