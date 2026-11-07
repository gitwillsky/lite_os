//! 网络控制、网络域与 socket 的使用者端。

use super::*;
use service_rpc::call;

/// 传输失败的领域投影：服务内存不足为 `NoMemory`，服务不可用（已终止）为 `NetworkDown`。
fn transport(status: Status) -> SocketError {
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
    pub fn socket(&self, protocol: InetProtocol) -> Result<SocketEndpoint, SocketError> {
        let mut writer = Writer::request(ordinal::SOCKET);
        writer.u8(match protocol {
            InetProtocol::Udp => 0,
            InetProtocol::Tcp => 1,
            InetProtocol::Icmp => 2,
            InetProtocol::Raw => 3,
        });
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
                name: fixed::<16>(reader)?,
                statistics: get_statistics(reader)?,
            })
        })
    }

    pub fn route_snapshot(&self) -> Result<Vec<RouteSnapshot>, SocketError> {
        self.rows(ordinal::ROUTE_SNAPSHOT, |reader| {
            Ok(RouteSnapshot {
                name: fixed::<16>(reader)?,
                route: get_route(reader)?,
            })
        })
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
}

impl RemoteInetSocket {
    pub fn new(channel: Channel) -> Self {
        Self { channel }
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

    pub fn send_to(
        &self,
        input: &[u8],
        target: Option<InetAddress>,
        packet_info: Option<InternetSendPacketInfo>,
    ) -> Result<usize, SocketError> {
        let mut writer = Writer::request(ordinal::SEND);
        put_option_inet(&mut writer, target);
        put_send_info(&mut writer, packet_info);
        let mut handles = Vec::new();
        put_payload(&mut writer, &mut handles, input).map_err(transport)?;
        let reply = request(&self.channel, &writer, handles)?;
        body(&reply)
            .u64()
            .map(|count| count as usize)
            .map_err(transport)
    }

    pub fn receive(&self, limit: usize, peek: bool) -> Result<InetReceived, SocketError> {
        let mut writer = Writer::request(ordinal::RECEIVE);
        writer.u64(limit as u64).bool(peek);
        let mut reply = request(&self.channel, &writer, Vec::new())?;
        let (full_length, source, packet_info, data) = {
            let (mut reader, handles) = reply.split();
            let _ = reader.u8();
            let full_length = reader.u64().map_err(transport)? as usize;
            let source = get_option_inet(&mut reader).map_err(transport)?;
            let packet_info = get_packet_info(&mut reader).map_err(transport)?;
            let data = get_payload(&mut reader, handles).map_err(transport)?;
            (full_length, source, packet_info, data)
        };
        Ok(InetReceived {
            data,
            full_length,
            source,
            packet_info,
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
        request(
            &self.channel,
            &Writer::request(ordinal::POLL_STATE),
            Vec::new(),
        )
        .and_then(|reply| get_poll_state(&mut body(&reply)).map_err(transport))
        .unwrap_or_else(|_| SocketPollState::error())
    }

    pub fn set_option(&self, change: InetOptionChange) -> Result<(), SocketError> {
        let mut writer = Writer::request(ordinal::SET_OPTION);
        put_option_change(&mut writer, change);
        self.unit(&writer)
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

    /// 关闭 socket（使用者最后一次文件关闭）；服务已终止时无事可做。
    pub fn close(&self) {
        let _ = call(&self.channel, &Writer::request(ordinal::CLOSE), Vec::new());
    }
}

/// 使用者端：一个 packet socket；drop 即关闭。
pub struct RemotePacketSocket {
    channel: Channel,
}

impl RemotePacketSocket {
    pub fn new(channel: Channel) -> Self {
        Self { channel }
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
        let mut writer = Writer::request(ordinal::SEND);
        writer.bool(target.is_some());
        if let Some(target) = target {
            put_packet_address(&mut writer, target);
        }
        let mut handles = Vec::new();
        put_payload(&mut writer, &mut handles, input).map_err(transport)?;
        let reply = request(&self.channel, &writer, handles)?;
        body(&reply)
            .u64()
            .map(|count| count as usize)
            .map_err(transport)
    }

    pub fn receive(&self, limit: usize, peek: bool) -> Result<PacketReceived, SocketError> {
        let mut writer = Writer::request(ordinal::RECEIVE);
        writer.u64(limit as u64).bool(peek);
        let mut reply = request(&self.channel, &writer, Vec::new())?;
        let (mut reader, handles) = reply.split();
        let _ = reader.u8();
        let full_length = reader.u64().map_err(transport)? as usize;
        let source = get_packet_address(&mut reader).map_err(transport)?;
        let data = get_payload(&mut reader, handles).map_err(transport)?;
        Ok(PacketReceived {
            data,
            full_length,
            source,
        })
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
        request(
            &self.channel,
            &Writer::request(ordinal::POLL_STATE),
            Vec::new(),
        )
        .and_then(|reply| get_poll_state(&mut body(&reply)).map_err(transport))
        .unwrap_or_else(|_| SocketPollState::error())
    }

    /// 关闭 socket（使用者最后一次文件关闭）；服务已终止时无事可做。
    pub fn close(&self) {
        let _ = call(&self.channel, &Writer::request(ordinal::CLOSE), Vec::new());
    }
}
