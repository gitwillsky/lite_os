//! 网络控制、网络域与 socket 的使用者端。

use super::*;
use service_rpc::call;

/// 传输失败的领域投影：服务内存不足为 `NoMemory`，服务不可用（已终止）为 `NetworkDown`。
pub(super) fn transport(status: Status) -> SocketError {
    if status == native_abi::ERR_NO_MEMORY {
        SocketError::NoMemory
    } else {
        SocketError::NetworkDown
    }
}

/// 发出请求并解出结果字节；领域失败与传输失败合并为 [`SocketError`]。
fn request(
    channel: &Channel,
    writer: &Writer,
    handles: Vec<Handle>,
) -> Result<service_rpc::Reply, SocketError> {
    let reply = call(channel, writer, handles).map_err(transport)?;
    get_result(&mut reply.reader())
        .map_err(transport)?
        .map(|()| reply)
}

/// 跳过结果字节后的回复读取器。
fn body(reply: &service_rpc::Reply) -> Reader<'_> {
    let mut reader = reply.reader();
    let _ = reader.u8();
    reader
}

fn take_endpoint(mut reply: service_rpc::Reply) -> Result<SocketEndpoint, SocketError> {
    let channel = Channel::from_handle(take_handle(&mut reply.handles, 0).map_err(transport)?);
    let readiness = Event::from_handle(take_handle(&mut reply.handles, 1).map_err(transport)?);
    Ok(SocketEndpoint { channel, readiness })
}

/// 使用者端：网络服务的控制 channel。
pub struct NetworkClient {
    channel: Channel,
}

impl NetworkClient {
    pub fn new(channel: Channel) -> Self {
        Self { channel }
    }

    /// 连接的 channel：服务终止时置位 `PEER_CLOSED`，使用者据此重连。
    pub fn channel(&self) -> &Channel {
        &self.channel
    }

    /// 创建网络域。
    ///
    /// # Errors
    ///
    /// 服务资源不足或已终止。
    pub fn create_namespace(&self, initial: bool) -> Result<RemoteNamespace, Status> {
        let mut writer = Writer::request(ordinal::CREATE_NAMESPACE);
        writer.bool(initial);
        let mut reply = call(&self.channel, &writer, Vec::new())?;
        Ok(RemoteNamespace {
            channel: Channel::from_handle(take_handle(&mut reply.handles, 0)?),
        })
    }
}

/// 使用者端：一个网络域；drop 即退休该域。
pub struct RemoteNamespace {
    channel: Channel,
}

impl RemoteNamespace {
    /// 接管上游授权服务移交的域连接。
    pub fn from_channel(channel: Channel) -> Self {
        Self { channel }
    }

    /// 移交此域连接给下游服务。
    pub fn into_channel(self) -> Channel {
        self.channel
    }

    /// 实际执行域的寿命连接；服务退出时 PEER_CLOSED，控制面据此丢弃旧接口身份并重建。
    pub fn channel(&self) -> &Channel {
        &self.channel
    }

    /// 借用同一个域的能力执行独立请求，避免把调用方状态锁持有到 RPC 完成。
    /// # Errors
    /// 能力复制失败。
    pub fn duplicate(&self) -> Result<Self, Status> {
        Ok(Self {
            channel: self.channel.duplicate(native_abi::rights::SAME)?,
        })
    }

    /// 创建本域的 `family`/`protocol` 端点，返回 channel 和就绪通知；不默认截断到 IPv4。
    /// # Errors
    /// 返回服务的协议/资源错误，或控制连接的传输错误。
    pub fn socket(
        &self,
        family: InetFamily,
        protocol: InetProtocol,
    ) -> Result<SocketEndpoint, SocketError> {
        if protocol == InetProtocol::Raw(0) {
            return Err(SocketError::ProtocolNotSupported);
        }
        let mut writer = Writer::request(ordinal::SOCKET);
        writer.u32(namespace::SOCKET_REQUEST_VERSION);
        writer.u8(match family {
            InetFamily::Ipv4 => 4,
            InetFamily::Ipv6 => 6,
        });
        writer.u8(match protocol {
            InetProtocol::Udp => 0,
            InetProtocol::Tcp => 1,
            InetProtocol::Raw(_) => 2,
        });
        if let InetProtocol::Raw(protocol) = protocol {
            writer.u8(protocol);
        }
        take_endpoint(request(&self.channel, &writer, Vec::new())?)
    }

    pub fn packet_socket(
        &self,
        kind: PacketKind,
        protocol: u16,
    ) -> Result<SocketEndpoint, SocketError> {
        let mut writer = Writer::request(ordinal::PACKET_SOCKET);
        writer.bool(kind == PacketKind::Raw).u16(protocol);
        take_endpoint(request(&self.channel, &writer, Vec::new())?)
    }

    pub fn interface_index(&self, name: &[u8]) -> Result<u32, SocketError> {
        let mut writer = Writer::request(ordinal::INTERFACE_INDEX);
        writer.bytes(name);
        let reply = request(&self.channel, &writer, Vec::new())?;
        body(&reply).u32().map_err(transport)
    }

    pub fn interface_snapshot(&self, index: u32) -> Result<InterfaceSnapshot, SocketError> {
        let mut writer = Writer::request(ordinal::INTERFACE_SNAPSHOT);
        writer.u32(index);
        let reply = request(&self.channel, &writer, Vec::new())?;
        get_interface(&mut body(&reply)).map_err(transport)
    }

    fn rows<T>(
        &self,
        ordinal: u32,
        decode: impl Fn(&mut Reader<'_>) -> Result<T, Status>,
    ) -> Result<Vec<T>, SocketError> {
        let reply = request(&self.channel, &Writer::request(ordinal), Vec::new())?;
        let mut reader = body(&reply);
        let count = reader.u32().map_err(transport)? as usize;
        let mut rows = Vec::new();
        rows.try_reserve_exact(count)
            .map_err(|_| SocketError::NoMemory)?;
        for _ in 0..count {
            rows.push(decode(&mut reader).map_err(transport)?);
        }
        Ok(rows)
    }

    pub fn interface_snapshots(&self) -> Result<Vec<InterfaceSnapshot>, SocketError> {
        self.rows(ordinal::INTERFACE_SNAPSHOTS, get_interface)
    }

    pub fn network_snapshot(&self) -> Result<Vec<NetworkSnapshot>, SocketError> {
        self.rows(ordinal::NETWORK_SNAPSHOT, |reader| {
            Ok(NetworkSnapshot {
                index: reader.u32()?,
                name: fixed::<16>(reader)?,
                statistics: get_statistics(reader)?,
            })
        })
    }

    /// 读取本域冻结的 IPv4 和 IPv6 邻居；使用 VMO，数量不受 channel 消息长度限制。
    /// # Parameters
    /// `index` 是原生逻辑接口，0 表示全部；未知非零接口由域 owner 返回 NoDevice。
    /// # Errors
    /// 服务/资源失败，或句柄、长度、版本与条目内容非法。
    pub fn neighbor_snapshot(&self, index: u32) -> Result<NeighborSnapshot, SocketError> {
        let mut writer = Writer::request(namespace::neighbors::ORDINAL);
        writer.u32(index);
        NeighborSnapshot::parse(self.snapshot_bytes(&writer)?).map_err(transport)
    }

    /// 读取管理邻居页的版本化正文。页内容由域 owner 在同一次读取中确定。
    /// # Errors
    /// 域、存储、能力或回复格式不合法。
    pub fn neighbor_page(
        &self,
        after: Option<router_protocol::neighbors::Key>,
    ) -> Result<Vec<u8>, SocketError> {
        let mut writer = Writer::request(namespace::NEIGHBOR_PAGE);
        writer.bytes(&router_protocol::neighbors::cursor(after));
        let bytes = self.snapshot_bytes(&writer)?;
        router_protocol::neighbors::Page::parse(&bytes, after).map_err(|_| SocketError::Invalid)?;
        Ok(bytes)
    }

    /// 读取生效路由页的完整正文，按所请求的族和位置验证。
    /// # Errors
    /// 域、存储、能力或回复格式不合法。
    pub fn route_page(
        &self,
        cursor: router_protocol::route_view::Cursor,
    ) -> Result<Vec<u8>, SocketError> {
        let mut writer = Writer::request(namespace::ROUTE_PAGE);
        writer.bytes(&cursor.encode());
        let bytes = self.snapshot_bytes(&writer)?;
        router_protocol::route_view::Page::parse(&bytes, cursor)
            .map_err(|_| SocketError::Invalid)?;
        Ok(bytes)
    }

    /// 读取本域冻结的接口地址；使用 VMO，数量不受 channel 消息长度限制。
    /// # Parameters
    /// `index` 是原生逻辑接口，0 表示全部；未知非零接口由域 owner 返回 NoDevice。
    /// # Errors
    /// 服务/资源失败，或句柄、长度、版本与条目内容非法。
    pub fn address_snapshot(&self, index: u32) -> Result<AddressSnapshot, SocketError> {
        let mut writer = Writer::request(namespace::addresses::ORDINAL);
        writer.u32(index);
        AddressSnapshot::parse(self.snapshot_bytes(&writer)?).map_err(transport)
    }

    /// 读取当前生效规则；`family` 为 None 时两族在同一次捕获中读取。
    /// # Errors
    /// 传输、资源、句柄或快照格式错误。
    pub fn rule_snapshot(
        &self,
        family: Option<InetFamily>,
    ) -> Result<routes::RuleSnapshot, SocketError> {
        let mut writer = Writer::request(namespace::RULE_SNAPSHOT);
        writer
            .u32(namespace::RULE_SNAPSHOT_VERSION)
            .u8(match family {
                None => 0,
                Some(InetFamily::Ipv4) => 4,
                Some(InetFamily::Ipv6) => 6,
            });
        routes::RuleSnapshot::parse(self.snapshot_bytes(&writer)?).map_err(transport)
    }

    fn snapshot_bytes(&self, writer: &Writer) -> Result<Vec<u8>, SocketError> {
        let mut reply = request(&self.channel, writer, Vec::new())?;
        let mut reader = body(&reply);
        let length = reader.u64().map_err(transport)?;
        reader.finish().map_err(transport)?;
        if reply.handles.len() != 1 {
            return Err(SocketError::Invalid);
        }
        Vmo::from_handle(take_handle(&mut reply.handles, 0).map_err(transport)?)
            .read_bytes(length)
            .map_err(transport)
    }

    /// 查询当前有效通告 DNS，拒绝无效正文或句柄。
    pub fn learned_dns(&self) -> Result<router_protocol::dns::LearnedDns, SocketError> {
        let writer = Writer::request(namespace::LEARNED_DNS);
        let mut reply = request(&self.channel, &writer, Vec::new())?;
        let mut reader = body(&reply);
        let length = reader.u64().map_err(transport)?;
        reader.finish().map_err(transport)?;
        if reply.handles.len() != 1 || length > router_protocol::dns::MAX_ENCODED_BYTES as u64 {
            return Err(SocketError::Invalid);
        }
        let bytes = Vmo::from_handle(take_handle(&mut reply.handles, 0).map_err(transport)?)
            .read_bytes(length)
            .map_err(transport)?;
        let mut decoder = router_protocol::Decoder::new(&bytes);
        let dns = decoder.learned_dns().map_err(|_| SocketError::Invalid)?;
        decoder.finish().map_err(|_| SocketError::Invalid)?;
        Ok(dns)
    }

    /// 登记 DNS 观察事件。观察者清除 USER_0 后重新装订，再查询当前快照。
    pub fn watch_dns(&self) -> Result<Event, SocketError> {
        let writer = Writer::request(namespace::WATCH_DNS);
        let mut reply = request(&self.channel, &writer, Vec::new())?;
        body(&reply).finish().map_err(transport)?;
        if reply.handles.len() != 1 {
            return Err(SocketError::Invalid);
        }
        Ok(Event::from_handle(
            take_handle(&mut reply.handles, 0).map_err(transport)?,
        ))
    }

    /// 读取完整冻结的 FIB；VMO 正文不受 channel 上限约束，成员按字典借用，不按行复制。
    /// # Errors
    /// 服务不可用、资源不足、句柄/长度/正文格式无效或服务拒绝。
    pub fn route_snapshot<A: routes::RouteAddress>(
        &self,
    ) -> Result<RoutingSnapshot<A>, SocketError> {
        let mut bodies = self.route_snapshot_contents(A::FAMILY as u8)?;
        RoutingSnapshot::parse(bodies.pop().expect("one family capture")).map_err(transport)
    }
    /// 在同一发布代际捕获两族，按 IPv4/IPv6 返回；不合并来自两次 RPC 的状态。
    /// # Errors
    /// 任一捕获失败、服务/传输/资源失败或正文非法，不返回部分集合。
    pub fn route_snapshots(
        &self,
    ) -> Result<
        (
            RoutingSnapshot<core::net::Ipv4Addr>,
            RoutingSnapshot<core::net::Ipv6Addr>,
        ),
        SocketError,
    > {
        let mut bodies = self.route_snapshot_contents(0)?;
        let six = bodies.pop().expect("IPv6 family capture");
        let four = bodies.pop().expect("IPv4 family capture");
        Ok((
            RoutingSnapshot::parse(four).map_err(transport)?,
            RoutingSnapshot::parse(six).map_err(transport)?,
        ))
    }
    /// 读取本域一次真实的输出或输入选路结果。
    /// # Errors
    /// 查询非法、源或设备不可用、无匹配路由、服务退出或回复非法。
    pub fn route_lookup(
        &self,
        query: routes::RouteLookup,
    ) -> Result<routes::RoutePath, SocketError> {
        let mut writer = Writer::request(namespace::ROUTE_LOOKUP);
        writer
            .try_reserve(routes::RouteLookup::MAX_BYTES)
            .map_err(transport)?;
        query.write(&mut writer).map_err(|_| SocketError::Invalid)?;
        let reply = request(&self.channel, &writer, Vec::new())?;
        if !reply.handles.is_empty() {
            return Err(SocketError::Invalid);
        }
        let mut reader = body(&reply);
        let path = routes::RoutePath::read(&mut reader, query).map_err(|_| SocketError::Invalid)?;
        reader.finish().map_err(|_| SocketError::Invalid)?;
        Ok(path)
    }

    fn route_snapshot_contents(&self, family: u8) -> Result<Vec<Vec<u8>>, SocketError> {
        let mut writer = Writer::request(ordinal::ROUTE_SNAPSHOT);
        writer.u32(namespace::ROUTE_SNAPSHOT_VERSION).u8(family);
        let mut reply = request(&self.channel, &writer, Vec::new())?;
        let mut reader = body(&reply);
        let count = if family == 0 { 2 } else { 1 };
        if reader.u32().map_err(transport)? != count || reply.handles.len() != count as usize {
            return Err(SocketError::Invalid);
        }
        let mut lengths = [0; 2];
        for length in lengths.iter_mut().take(count as usize) {
            *length = reader.u64().map_err(transport)?;
        }
        reader.finish().map_err(transport)?;
        let mut bodies = Vec::new();
        bodies
            .try_reserve_exact(count as usize)
            .map_err(|_| SocketError::NoMemory)?;
        for length in lengths.into_iter().take(count as usize) {
            bodies.push(
                Vmo::from_handle(take_handle(&mut reply.handles, 0).map_err(transport)?)
                    .read_bytes(length)
                    .map_err(transport)?,
            );
        }
        Ok(bodies)
    }

    fn configure(
        &self,
        ordinal: u32,
        index: u32,
        encode: impl FnOnce(&mut Writer),
    ) -> Result<(), SocketError> {
        let mut writer = Writer::request(ordinal);
        writer.u32(index);
        encode(&mut writer);
        request(&self.channel, &writer, Vec::new()).map(drop)
    }

    pub fn configure_mtu(&self, index: u32, mtu: i32) -> Result<(), SocketError> {
        self.configure(ordinal::CONFIGURE_MTU, index, |writer| {
            writer.i32(mtu);
        })
    }

    pub fn configure_address(&self, index: u32, address: Ipv4Addr) -> Result<(), SocketError> {
        self.configure(ordinal::CONFIGURE_ADDRESS, index, |writer| {
            put_ipv4(writer, address)
        })
    }

    pub fn configure_netmask(&self, index: u32, netmask: Ipv4Addr) -> Result<(), SocketError> {
        self.configure(ordinal::CONFIGURE_NETMASK, index, |writer| {
            put_ipv4(writer, netmask)
        })
    }

    pub fn configure_broadcast(&self, index: u32, broadcast: Ipv4Addr) -> Result<(), SocketError> {
        self.configure(ordinal::CONFIGURE_BROADCAST, index, |writer| {
            put_ipv4(writer, broadcast)
        })
    }

    pub fn configure_up(&self, index: u32, up: bool) -> Result<(), SocketError> {
        self.configure(ordinal::CONFIGURE_UP, index, |writer| {
            writer.bool(up);
        })
    }

    /// # Errors
    ///
    /// FIB 准入失败；服务不可用时为 `NetworkUnreachable`，内存不足为 `NoMemory`。
    pub fn configure_route(
        &self,
        route: Ipv4RouteRequest,
        change: RouteChange,
    ) -> Result<(), Ipv4RouteError> {
        let mut writer = Writer::request(ordinal::CONFIGURE_ROUTE);
        put_route(&mut writer, &route);
        writer
            .bool(change.delete)
            .bool(change.replace)
            .bool(change.create)
            .bool(change.exclusive)
            .bool(change.append);
        let unavailable = |status| {
            if status == native_abi::ERR_NO_MEMORY {
                Ipv4RouteError::NoMemory
            } else {
                Ipv4RouteError::NetworkUnreachable
            }
        };
        let reply = call(&self.channel, &writer, Vec::new()).map_err(unavailable)?;
        match reply.reader().u8().map_err(unavailable)? {
            0 => Ok(()),
            code => Err(Ipv4RouteError::from_code(code).ok_or(Ipv4RouteError::Invalid)?),
        }
    }
}

fn get_error(reply: &service_rpc::Reply) -> Option<SocketError> {
    reply.reader().u8().ok().and_then(SocketError::from_code)
}

/// 使用者端：一个 INET socket；drop 即关闭。
pub struct RemoteInetSocket {
    channel: Channel,
    data: client_queue::ClientQueue,
}

impl RemoteInetSocket {
    /// 用已授权的 socket channel 协商共享数据面，返回持有 channel 与固定队列的客户端。
    ///
    /// # Errors
    ///
    /// 原生对象不足为 `NoMemory`；服务终止、版本不匹配或协商失败为 `NetworkDown`。
    pub fn new(channel: Channel) -> Result<Self, SocketError> {
        let data = client_queue::ClientQueue::connect(&channel).map_err(transport)?;
        Ok(Self { channel, data })
    }

    fn unit(&self, writer: &Writer) -> Result<(), SocketError> {
        request(&self.channel, writer, Vec::new()).map(drop)
    }

    pub fn bind(&self, address: InetAddress) -> Result<(), SocketError> {
        let mut writer = Writer::request(ordinal::BIND);
        put_inet(&mut writer, address);
        self.unit(&writer)
    }

    pub fn connect(&self, peer: InetAddress) -> Result<(), SocketError> {
        let mut writer = Writer::request(ordinal::CONNECT);
        put_inet(&mut writer, peer);
        self.unit(&writer)
    }

    pub fn disconnect(&self) -> Result<(), SocketError> {
        self.unit(&Writer::request(ordinal::DISCONNECT))
    }

    pub fn address(&self) -> Result<InetAddress, SocketError> {
        let reply = request(
            &self.channel,
            &Writer::request(ordinal::ADDRESS),
            Vec::new(),
        )?;
        get_inet(&mut body(&reply)).map_err(transport)
    }

    pub fn peer_address(&self) -> Result<InetAddress, SocketError> {
        let reply = request(
            &self.channel,
            &Writer::request(ordinal::PEER_ADDRESS),
            Vec::new(),
        )?;
        get_inet(&mut body(&reply)).map_err(transport)
    }

    /// 发送应用数据和按原顺序保留的单次控制项，不修改 socket 绑定。
    ///
    /// # Parameters
    /// - `input`：正文；控制前缀与正文共用一个有界载荷槽。
    /// - `target`：显式目的；None 使用已关联 peer。
    /// - `packet_info`：精确计数的有序控制项，按实际目的族验证。
    ///
    /// # Returns
    /// 实际发送字节数；datagram 保持报文原子性。
    ///
    /// # Errors
    /// 正文过大返回 MessageTooLarge，前缀合计超限返回 NoBuffer；其余由实际传输与服务状态决定。
    pub fn send_to(
        &self,
        input: &[u8],
        target: Option<InetAddress>,
        packet_info: impl ExactSizeIterator<Item = InternetSendPacketInfo>,
    ) -> Result<usize, SocketError> {
        if input.len() > TRANSFER_LIMIT {
            return Err(SocketError::MessageTooLarge);
        }
        socket_io::payload_length(input.len(), packet_info.len())
            .map_err(|_| SocketError::NoBuffer)?;
        let mut record = socket_io::request(socket_io::SEND, input.len(), false);
        socket_io::inet(&mut record, target);
        record[8] = packet_info.len() as u64;
        let (response, _) = self
            .data
            .transfer(record, input, packet_info)
            .map_err(transport)?;
        socket_io::result(&response)?;
        Ok(response[2] as usize)
    }

    pub fn receive(&self, limit: usize, peek: bool) -> Result<InetReceived, SocketError> {
        let record = socket_io::request(socket_io::RECEIVE, limit, peek);
        let (response, data) = self
            .data
            .transfer(record, &[], core::iter::empty())
            .map_err(transport)?;
        socket_io::result(&response)?;
        Ok(InetReceived {
            data,
            full_length: response[3] as usize,
            source: socket_io::get_inet(&response).map_err(transport)?,
            packet_info: socket_io::get_receive_info(&response).map_err(transport)?,
        })
    }

    /// 非阻塞消费原生错误队列中的一条记录。
    /// # Errors
    /// 空队列为 Again。传输、分配失败和不支持的 socket 返回对应错误。
    pub fn receive_error(&self, limit: usize) -> Result<InetErrorReceived, SocketError> {
        let record = socket_io::request(socket_io::RECEIVE_ERROR, limit, false);
        let (response, data) = self
            .data
            .transfer(record, &[], core::iter::empty())
            .map_err(transport)?;
        socket_io::result(&response)?;
        Ok(InetErrorReceived {
            data,
            full_length: response[3] as usize,
            error: socket_io::get_error(&response).map_err(transport)?,
        })
    }

    pub fn is_listening(&self) -> Result<bool, SocketError> {
        let reply = request(
            &self.channel,
            &Writer::request(ordinal::IS_LISTENING),
            Vec::new(),
        )?;
        body(&reply).bool().map_err(transport)
    }

    pub fn listen(&self, backlog: usize) -> Result<(), SocketError> {
        let mut writer = Writer::request(ordinal::LISTEN);
        writer.u64(backlog as u64);
        self.unit(&writer)
    }

    pub fn accept(&self) -> Result<SocketEndpoint, SocketError> {
        take_endpoint(request(
            &self.channel,
            &Writer::request(ordinal::ACCEPT),
            Vec::new(),
        )?)
    }

    pub fn connection_result(&self) -> Result<(), SocketError> {
        self.unit(&Writer::request(ordinal::CONNECTION_RESULT))
    }

    pub fn take_error(&self) -> Option<SocketError> {
        match call(
            &self.channel,
            &Writer::request(ordinal::TAKE_ERROR),
            Vec::new(),
        ) {
            Ok(reply) => get_error(&reply),
            Err(status) => Some(transport(status)),
        }
    }

    pub fn shutdown(&self, how: usize) -> Result<(), SocketError> {
        let mut writer = Writer::request(ordinal::SHUTDOWN);
        writer.u64(how as u64);
        self.unit(&writer)
    }

    /// 服务不可用时报告错误状态。
    pub fn poll_state(&self) -> SocketPollState {
        self.data
            .transfer(
                socket_io::request(socket_io::POLL, 0, false),
                &[],
                core::iter::empty(),
            )
            .map_err(transport)
            .and_then(|(response, _)| {
                socket_io::result(&response)?;
                socket_io::get_poll(&response).map_err(transport)
            })
            .unwrap_or_else(|_| SocketPollState::error())
    }

    pub fn set_option(&self, change: InetOptionChange) -> Result<(), SocketError> {
        let mut writer = Writer::request(ordinal::SET_OPTION);
        writer.try_reserve(40).map_err(transport)?;
        put_option_change(&mut writer, change);
        self.unit(&writer)
    }

    /// Add/replace a TCP peer signature key, or delete it with an empty byte slice.
    /// # Errors
    /// Invalid selector/length, missing deletion target, allocation or service failure.
    pub fn set_md5_key(&self, selector: TcpKeySelector, key: &[u8]) -> Result<(), SocketError> {
        if !selector.valid() || key.len() > TCP_MD5_MAX_KEY_BYTES {
            return Err(SocketError::Invalid);
        }
        let mut writer = Writer::request(ordinal::MD5_KEY);
        writer.try_reserve(128).map_err(transport)?;
        writer.u32(1);
        options::put_tcp_key_selector(&mut writer, selector);
        writer.bytes(key);
        self.unit(&writer)
    }

    /// Install or remove a TCP-AO master key on this native TCP socket.
    /// # Errors
    /// Invalid configuration, allocation failure or a rejected socket policy update.
    pub fn set_ao_key(
        &self,
        config: TcpAoKey,
        key: &[u8],
        selection: TcpAoSelection,
    ) -> Result<(), SocketError> {
        let selector = config.selector;
        if !selector.valid() || key.len() > TCP_AO_MAX_KEY_BYTES {
            return Err(SocketError::Invalid);
        }
        let mut writer = Writer::request(ordinal::AO_KEY);
        writer.try_reserve(136).map_err(transport)?;
        writer.u32(2);
        options::put_tcp_key_selector(&mut writer, selector);
        options::put_ao_key(&mut writer, config, key);
        selection.write(&mut writer);
        self.unit(&writer)
    }

    /// Replace one exact peer's nonempty key set without rebuilding this socket.
    /// `selection` may change the selected IDs in the same operation as retiring keys.
    /// # Errors
    /// Inconsistent selectors, invalid keys or state, conflicting policy or resource failure.
    pub fn replace_ao_keys(
        &self,
        keys: &[(TcpAoKey, &[u8])],
        selection: TcpAoSelection,
    ) -> Result<(), SocketError> {
        let selector = keys.first().ok_or(SocketError::Invalid)?.0.selector;
        if !selector.valid()
            || keys.len() > 256
            || keys.iter().any(|(config, key)| {
                config.selector != selector || key.is_empty() || key.len() > TCP_AO_MAX_KEY_BYTES
            })
        {
            return Err(SocketError::Invalid);
        }
        let mut writer = Writer::request(options::AO_REPLACE_KEYS);
        writer
            .try_reserve(48 + 88 * keys.len())
            .map_err(transport)?;
        writer.u32(1);
        options::put_tcp_key_selector(&mut writer, selector);
        selection.write(&mut writer);
        writer.u16(keys.len() as u16);
        for &(config, key) in keys {
            options::put_ao_key(&mut writer, config, key);
        }
        self.unit(&writer)
    }

    /// Snapshot selected IDs and authentication counters without reading any secret bytes.
    /// # Errors
    /// TCP-AO has not been configured, the socket is closed, or the control exchange fails.
    pub fn ao_status(&self) -> Result<TcpAoStatus, SocketError> {
        let mut writer = Writer::request(ordinal::AO_STATUS);
        writer.try_reserve(4).map_err(transport)?;
        writer.u32(1);
        let reply = request(&self.channel, &writer, Vec::new())?;
        TcpAoStatus::read(&mut body(&reply)).map_err(transport)
    }

    /// Read the ICMP message-type filter, or atomically change its masked bits.
    /// # Parameters
    /// `update`: None reads; Some merges only the selected bits without a read/modify/write race.
    /// # Returns
    /// The complete eight-word filter after this operation; a set bit blocks that message type.
    /// # Errors
    /// Unsupported socket, malformed response, allocation or service failure.
    pub fn icmp_filter(&self, update: Option<IcmpFilterUpdate>) -> Result<[u32; 8], SocketError> {
        let mut writer = Writer::request(ordinal::ICMP_FILTER);
        writer.try_reserve(69).map_err(transport)?;
        writer.u32(1).bool(update.is_some());
        if let Some(update) = update {
            for (mask, blocked) in update.mask.into_iter().zip(update.blocked) {
                writer.u32(mask).u32(blocked);
            }
        }
        let reply = request(&self.channel, &writer, Vec::new())?;
        let mut reader = body(&reply);
        let mut values = [0; 8];
        for value in &mut values {
            *value = reader.u32().map_err(transport)?;
        }
        reader.finish().map_err(transport)?;
        Ok(values)
    }

    pub fn bind_to_device(&self, name: &[u8], privileged: bool) -> Result<(), SocketError> {
        let mut writer = Writer::request(ordinal::BIND_TO_DEVICE);
        writer.bytes(name).bool(privileged);
        self.unit(&writer)
    }

    pub fn option(&self, option: InternetOption) -> Result<i32, SocketError> {
        let mut writer = Writer::request(ordinal::OPTION);
        writer.u8(option.code());
        let reply = request(&self.channel, &writer, Vec::new())?;
        body(&reply).i32().map_err(transport)
    }

    /// 查询实际 TCP 阶段及发送统计。
    /// # Errors
    /// 端点不可用、传输协议不符或服务回复无效。
    pub fn tcp_information(&self) -> Result<TcpInformation, SocketError> {
        let mut writer = Writer::request(TcpInformation::ORDINAL);
        writer.u32(TcpInformation::VERSION);
        let reply = request(&self.channel, &writer, Vec::new())?;
        let mut input = body(&reply);
        let info = TcpInformation::read(&mut input).map_err(transport)?;
        input.finish().map_err(transport)?;
        Ok(info)
    }

    /// 关闭 socket（使用者最后一次文件关闭）；服务已终止时无事可做。
    pub fn close(&self) {
        let _ = call(&self.channel, &Writer::request(ordinal::CLOSE), Vec::new());
    }
}

/// 使用者端：一个 packet socket；drop 即关闭。
pub struct RemotePacketSocket {
    channel: Channel,
    data: client_queue::ClientQueue,
}

impl RemotePacketSocket {
    /// 用已授权的 socket channel 协商共享数据面，返回持有 channel 与固定队列的客户端。
    ///
    /// # Errors
    ///
    /// 原生对象不足为 `NoMemory`；服务终止、版本不匹配或协商失败为 `NetworkDown`。
    pub fn new(channel: Channel) -> Result<Self, SocketError> {
        let data = client_queue::ClientQueue::connect(&channel).map_err(transport)?;
        Ok(Self { channel, data })
    }

    pub fn bind(&self, address: PacketAddress) -> Result<(), SocketError> {
        let mut writer = Writer::request(ordinal::BIND);
        put_packet_address(&mut writer, address);
        request(&self.channel, &writer, Vec::new()).map(drop)
    }

    pub fn address(&self) -> Result<PacketAddress, SocketError> {
        let reply = request(
            &self.channel,
            &Writer::request(ordinal::ADDRESS),
            Vec::new(),
        )?;
        get_packet_address(&mut body(&reply)).map_err(transport)
    }

    pub fn send_to(
        &self,
        input: &[u8],
        target: Option<PacketAddress>,
    ) -> Result<usize, SocketError> {
        if input.len() > TRANSFER_LIMIT {
            return Err(SocketError::MessageTooLarge);
        }
        let mut record = socket_io::request(socket_io::SEND, input.len(), false);
        socket_io::packet(&mut record, target);
        let (response, _) = self
            .data
            .transfer(record, input, core::iter::empty())
            .map_err(transport)?;
        socket_io::result(&response)?;
        Ok(response[2] as usize)
    }

    pub fn receive(&self, limit: usize, peek: bool) -> Result<PacketReceived, SocketError> {
        let record = socket_io::request(socket_io::RECEIVE, limit, peek);
        let (response, data) = self
            .data
            .transfer(record, &[], core::iter::empty())
            .map_err(transport)?;
        socket_io::result(&response)?;
        Ok(PacketReceived {
            data,
            full_length: response[3] as usize,
            source: socket_io::get_packet(&response)
                .map_err(transport)?
                .ok_or(SocketError::NetworkDown)?,
            information: socket_io::get_packet_information(&response).map_err(transport)?,
        })
    }

    /// 返回并清零匹配数和丢弃数。
    /// # Errors
    /// endpoint 已关闭或服务回复无效。
    pub fn take_statistics(&self) -> Result<[u32; 2], SocketError> {
        let reply = request(
            &self.channel,
            &Writer::request(crate::packet::STATISTICS),
            Vec::new(),
        )?;
        let mut reader = body(&reply);
        let values = [
            reader.u32().map_err(transport)?,
            reader.u32().map_err(transport)?,
        ];
        reader.finish().map_err(transport)?;
        Ok(values)
    }

    /// 安装固定帧接收环，或恢复普通接收队列。
    ///
    /// # Errors
    /// 几何无效、缓冲无法建立、已有接收环，或远端已关闭。
    pub fn set_receive_ring(
        &self,
        ring: Option<(FrameRingLayout, Vmo)>,
    ) -> Result<(), SocketError> {
        let mut writer = Writer::request(crate::packet::RECEIVE_RING);
        writer
            .try_reserve(4 + 1 + FrameRingLayout::WIRE_BYTES)
            .map_err(transport)?;
        writer.u32(FrameRingLayout::VERSION).bool(ring.is_some());
        let mut handles = Vec::new();
        if let Some((layout, vmo)) = ring {
            layout.write(&mut writer);
            handles
                .try_reserve_exact(1)
                .map_err(|_| SocketError::NoMemory)?;
            handles.push(vmo.into_handle());
        }
        let reply = request(&self.channel, &writer, handles)?;
        body(&reply).finish().map_err(transport)
    }

    pub fn take_error(&self) -> Option<SocketError> {
        match call(
            &self.channel,
            &Writer::request(ordinal::TAKE_ERROR),
            Vec::new(),
        ) {
            Ok(reply) => get_error(&reply),
            Err(status) => Some(transport(status)),
        }
    }

    pub fn poll_state(&self) -> SocketPollState {
        self.data
            .transfer(
                socket_io::request(socket_io::POLL, 0, false),
                &[],
                core::iter::empty(),
            )
            .map_err(transport)
            .and_then(|(response, _)| {
                socket_io::result(&response)?;
                socket_io::get_poll(&response).map_err(transport)
            })
            .unwrap_or_else(|_| SocketPollState::error())
    }

    /// 关闭 socket（使用者最后一次文件关闭）；服务已终止时无事可做。
    pub fn close(&self) {
        let _ = call(&self.channel, &Writer::request(ordinal::CLOSE), Vec::new());
    }
}
