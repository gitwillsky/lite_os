//! `NETLINK_ROUTE` endpoint：`getifaddrs`、`ip link/addr/route/neigh/rule` 使用的网络状态查询。
//!
//! # 惰性 dump
//!
//! dump 请求到达时只冻结一份网络快照和游标；
//! 回复在 `recvmsg` 时按接收缓冲区容量逐个数据报生成，最后以 `NLMSG_DONE` 结束。与 Linux 一样，
//! 回复不预先排队，队列内存有界，不需要接收预算；快照冻结保证一次 dump 内部一致。
//!
//! # 范围
//!
//! 支持设备、地址、路由、邻居和规则查询。两族 `RTM_GETROUTE` 支持 dump 及双向路径点查。
//! 配置写入归统一配置 owner，Linux 侧按权限/受管约束拒绝；多播订阅尚未开放。

use crate::sync::Mutex;
use alloc::{collections::VecDeque, sync::Arc, vec::Vec};

use crate::ipc::{Pipe, PipeDirection, PipeEnd, ReceiveBuffer};
use device_model::network::NetworkStatistics;
use fallible_tree::FallibleMap;

use super::inet::rule_snapshot;
use super::{
    CoreBufferLimit, InterfaceSnapshot, NetlinkAddress, ReceivedMessage, SocketAddress,
    SocketError, SocketIoDirection, SocketMemory, SocketPollState, address_snapshot,
    interface_snapshots, kobject, neighbor_snapshot, network_snapshot, route_lookup,
    route_snapshot, route_snapshots, socket_errno,
};

mod change;
mod codec;

/// `NETLINK_ROUTE` 的协议号，作为 port 空间 key。
const PROTOCOL_ROUTE: u8 = 0;
/// dump 的打包目标；首条完整消息可超过该窗口，用户缓冲不足时按 Linux MSG_TRUNC 截断。
const MAX_DATAGRAM: usize = 16384;
/// 一次 sendmsg 内允许排队的 ACK/错误数据报个数，超出按 netlink overrun 处理。
const MAX_CONTROL_DATAGRAMS: usize = 64;
const AF_UNSPEC: u8 = 0;
const AF_INET: u8 = 2;
const AF_INET6: u8 = 10;
const ENOMEM: i32 = 12;
const ENODEV: i32 = 19;
const EBUSY: i32 = 16;
const EINVAL: i32 = 22;
const EOPNOTSUPP: i32 = 95;

enum Rows {
    Empty,
    Links(Vec<(InterfaceSnapshot, NetworkStatistics)>),
    Addresses {
        family: u8,
        rows: network_protocol::AddressSnapshot,
    },
    Routes {
        ipv4: Option<codec::RouteDump<core::net::Ipv4Addr>>,
        ipv6: Option<codec::RouteDump<core::net::Ipv6Addr>>,
    },
    Rules(network_protocol::routes::RuleSnapshot),
    Neighbors {
        rows: network_protocol::NeighborSnapshot,
        interface: u32,
        family: u8,
    },
}

impl Rows {
    fn maximum_message_size(&self) -> usize {
        match self {
            Self::Routes { ipv4, ipv6 } => ipv4
                .as_ref()
                .map_or(0, |rows| rows.maximum_message_size())
                .max(ipv6.as_ref().map_or(0, |rows| rows.maximum_message_size())),
            _ => 1024,
        }
    }
    fn len(&self) -> usize {
        match self {
            Self::Empty => 0,
            Self::Links(rows) => rows.len(),
            Self::Addresses { rows, .. } => rows.len(),
            Self::Routes { ipv4, ipv6 } => {
                ipv4.as_ref().map_or(0, |rows| rows.len())
                    + ipv6.as_ref().map_or(0, |rows| rows.len())
            }
            Self::Rules(rows) => rows.len(),
            Self::Neighbors { rows, .. } => rows.len(),
        }
    }
}

/// 一次进行中的 dump：冻结的行与游标。`next == rows.len()` 之后只剩 `NLMSG_DONE`。
struct Dump {
    rows: Rows,
    next: usize,
    sequence: u32,
}

struct RouteSocketState {
    address: Option<NetlinkAddress>,
    // OWNER: ACK/错误等单次回复；dump 回复不入队，由 `dump` 惰性生成。
    control: VecDeque<Vec<u8>>,
    dump: Option<Dump>,
    receive_limit: usize,
    // 发送缓冲只是被记录并回读的值：请求在 send 里同步处理，不存在发送队列可以限额。
    send_limit: usize,
    // OWNER: overrun 错误与队列分开；SO_ERROR 消费错误不丢已排队的回复。
    pending_error: bool,
}

/// `NETLINK_ROUTE` actual NET endpoint。
pub(super) struct RouteSocket {
    id: u64,
    memory: Arc<SocketMemory>,
    // OWNER: Some 是 live endpoint 的唯一生命周期；None 表示 fput 已退休。
    state: Mutex<Option<RouteSocketState>>,
    notify_read: Arc<PipeEnd>,
    notify_write: Arc<PipeEnd>,
}

impl RouteSocket {
    pub(super) fn new(
        notify: (Arc<PipeEnd>, Arc<PipeEnd>),
        memory: Arc<SocketMemory>,
    ) -> Result<Arc<Self>, SocketError> {
        Arc::try_new(Self {
            id: crate::id::next_runtime_object_id(),
            memory,
            state: Mutex::new(Some(RouteSocketState {
                address: None,
                control: VecDeque::new(),
                dump: None,
                receive_limit: CoreBufferLimit::ReceiveDefault.value() as usize,
                send_limit: CoreBufferLimit::SendDefault.value() as usize,
                pending_error: false,
            })),
            notify_read: notify.0,
            notify_write: notify.1,
        })
        .map_err(|_| SocketError::NoMemory)
    }

    /// Linux bind：port 空间按（协议，NET）隔离，0 由调用者的可见 TGID 自动选择；不支持多播 group。
    pub(super) fn bind(
        &self,
        address: NetlinkAddress,
        caller_tgid: u32,
    ) -> Result<(), SocketError> {
        if address.groups != 0 {
            return Err(SocketError::OperationNotSupported);
        }
        let net = self.memory.namespace_ref().id();
        // OOM 不改变既有绑定：索引节点先于任何发表准备好。
        let prepared = FallibleMap::try_prepare((PROTOCOL_ROUTE, net, 0), self.id)
            .map_err(|_| SocketError::NoMemory)?;
        // 加锁顺序固定为 state -> port registry；close 不在持有 registry 时取 state。
        let mut guard = self.state.lock();
        let state = guard.as_mut().ok_or(SocketError::NotConnected)?;
        if let Some(bound) = state.address {
            return if address.port_id == bound.port_id {
                Ok(())
            } else {
                Err(SocketError::Invalid)
            };
        }
        let port = kobject::with_ports(|ports| {
            let port = kobject::choose_port(
                ports,
                PROTOCOL_ROUTE,
                net,
                self.id,
                address.port_id,
                caller_tgid,
            )?;
            let mut entry = prepared;
            entry.set_key((PROTOCOL_ROUTE, net, port));
            ports.commit_vacant(entry);
            Ok(port)
        })?;
        state.address = Some(NetlinkAddress {
            port_id: port,
            groups: 0,
        });
        Ok(())
    }

    /// 首次发送前的 autobind（Linux `netlink_autobind`）；已绑定时无操作。
    pub(super) fn autobind(&self, caller_tgid: u32) -> Result<(), SocketError> {
        let bound = self
            .state
            .lock()
            .as_ref()
            .ok_or(SocketError::NotConnected)?
            .address
            .is_some();
        if bound {
            return Ok(());
        }
        self.bind(
            NetlinkAddress {
                port_id: 0,
                groups: 0,
            },
            caller_tgid,
        )
    }

    pub(super) fn address(&self) -> NetlinkAddress {
        self.state
            .lock()
            .as_ref()
            .and_then(|state| state.address)
            .unwrap_or(NetlinkAddress {
                port_id: 0,
                groups: 0,
            })
    }

    /// 解析并执行数据报里的全部请求。
    ///
    /// # Parameters
    ///
    /// - `input`: 一个或多个连续的 nlmsghdr 请求。
    /// - `destination`: 显式目的地址；只允许内核（port 0、无 group）。
    ///
    /// # Returns
    ///
    /// 消费的字节数；回复通过 `receive` 取走。
    pub(super) fn send(
        &self,
        input: &[u8],
        destination: Option<NetlinkAddress>,
        net_admin: bool,
    ) -> Result<usize, SocketError> {
        if let Some(destination) = destination {
            if destination.groups != 0 {
                return Err(SocketError::OperationNotPermitted);
            }
            if destination.port_id != 0 {
                // 同协议里没有该 port 的用户态 endpoint。
                return Err(SocketError::ConnectionRefused);
            }
        }
        let port = self
            .state
            .lock()
            .as_ref()
            .ok_or(SocketError::NotConnected)?
            .address
            .map_or(0, |address| address.port_id);
        let mut control = VecDeque::new();
        let mut dump = None;
        let mut offset = 0;
        while let Some(header) = codec::parse_header(&input[offset..]) {
            let message = &input[offset..offset + header.length];
            let dump_before = dump.is_some();
            let reply = self.handle_request(&header, message, port, net_admin, &mut dump);
            let errored = matches!(reply, Reply::Error(_));
            match reply {
                Reply::None => {}
                Reply::Datagram(bytes) => {
                    control.try_reserve(1).map_err(|_| SocketError::NoMemory)?;
                    control.push_back(bytes);
                }
                Reply::Error(code) => {
                    let mut bytes = Vec::new();
                    codec::error(code, message, header.sequence, port, &mut bytes)
                        .map_err(|()| SocketError::NoMemory)?;
                    control.try_reserve(1).map_err(|_| SocketError::NoMemory)?;
                    control.push_back(bytes);
                }
            }
            // NLM_F_ACK：没有出错、也没有开始 dump（dump 以 NLMSG_DONE 收尾）时补发 error = 0 的确认。
            let started_dump = !dump_before && dump.is_some();
            if header.flags & codec::NLM_F_ACK != 0 && !errored && !started_dump {
                let mut bytes = Vec::new();
                codec::error(0, message, header.sequence, port, &mut bytes)
                    .map_err(|()| SocketError::NoMemory)?;
                control.try_reserve(1).map_err(|_| SocketError::NoMemory)?;
                control.push_back(bytes);
            }
            offset += codec::next_offset(header.length);
            if offset >= input.len() {
                break;
            }
        }
        self.install(control, dump);
        Ok(input.len())
    }

    /// 处理一条请求，必要时在 `dump` 里放入新的 dump。
    fn handle_request(
        &self,
        header: &codec::RequestHeader,
        message: &[u8],
        port: u32,
        net_admin: bool,
        dump: &mut Option<Dump>,
    ) -> Reply {
        // 不是请求或属于控制类型的消息只可能要求 ACK，没有其他语义。
        if header.flags & codec::NLM_F_REQUEST == 0 || header.kind < codec::NLMSG_MIN_TYPE {
            return Reply::None;
        }
        if header.kind > codec::RTM_MAX {
            return Reply::Error(-EOPNOTSUPP);
        }
        // rtgenmsg 至少一个字节；更短的请求被 Linux 静默忽略。
        if header.length < codec::HEADER_LENGTH + 1 {
            return Reply::None;
        }
        let payload = &message[codec::HEADER_LENGTH..];
        let is_dump = header.flags & codec::NLM_F_DUMP == codec::NLM_F_DUMP;
        let scope = self.memory.namespace_ref();
        let family = payload[0];
        match header.kind {
            codec::RTM_GETLINK if is_dump => {
                if dump.is_some() {
                    return Reply::Error(-EBUSY);
                }
                match collect_links(scope) {
                    Ok(rows) => {
                        *dump = Some(new_dump(Rows::Links(rows), header.sequence));
                        Reply::None
                    }
                    Err(error) => Reply::Error(-socket_errno(error)),
                }
            }
            codec::RTM_GETLINK => self.single_link(scope, header, payload, port),
            codec::RTM_GETADDR if is_dump => {
                if dump.is_some() {
                    return Reply::Error(-EBUSY);
                }
                if !matches!(family, AF_UNSPEC | AF_INET | AF_INET6) {
                    *dump = Some(new_dump(Rows::Empty, header.sequence));
                    return Reply::None;
                }
                match address_snapshot(scope, 0) {
                    Ok(rows) => match codec::validate_addresses(&rows, family) {
                        Ok(()) => {
                            *dump =
                                Some(new_dump(Rows::Addresses { family, rows }, header.sequence));
                            Reply::None
                        }
                        Err(error) => Reply::Error(-error),
                    },
                    Err(error) => Reply::Error(-socket_errno(error)),
                }
            }
            codec::RTM_GETROUTE if is_dump => {
                if dump.is_some() {
                    return Reply::Error(-EBUSY);
                }
                let rows = (|| {
                    if !matches!(family, AF_UNSPEC | AF_INET | AF_INET6) {
                        return Ok(Rows::Empty);
                    }
                    if family == AF_UNSPEC {
                        let (four, six) =
                            route_snapshots(scope).map_err(|error| -socket_errno(error))?;
                        return Ok(Rows::Routes {
                            ipv4: Some(codec::RouteDump::new(four)?),
                            ipv6: Some(codec::RouteDump::new(six)?),
                        });
                    }
                    let ipv4 = if family == AF_INET {
                        Some(codec::RouteDump::new(
                            route_snapshot::<core::net::Ipv4Addr>(scope)
                                .map_err(|error| -socket_errno(error))?,
                        )?)
                    } else {
                        None
                    };
                    let ipv6 = if family == AF_INET6 {
                        Some(codec::RouteDump::new(
                            route_snapshot::<core::net::Ipv6Addr>(scope)
                                .map_err(|error| -socket_errno(error))?,
                        )?)
                    } else {
                        None
                    };
                    Ok(Rows::Routes { ipv4, ipv6 })
                })();
                match rows {
                    Ok(rows) => {
                        *dump = Some(new_dump(rows, header.sequence));
                        Reply::None
                    }
                    Err(error) => Reply::Error(error),
                }
            }
            codec::RTM_GETROUTE => {
                let result = (|| {
                    let query = codec::route_query(payload)?;
                    let path = route_lookup(scope, query).map_err(socket_errno)?;
                    codec::route_path(query, path, header.sequence, port)
                })();
                match result {
                    Ok(datagram) => Reply::Datagram(datagram),
                    Err(error) => Reply::Error(-error),
                }
            }
            codec::RTM_GETRULE if is_dump => {
                if dump.is_some() {
                    return Reply::Error(-EBUSY);
                }
                let result = codec::rule_query(payload)
                    .and_then(|family| rule_snapshot(scope, family).map_err(socket_errno));
                match result {
                    Ok(rows) => {
                        *dump = Some(new_dump(Rows::Rules(rows), header.sequence));
                        Reply::None
                    }
                    Err(error) => Reply::Error(-error),
                }
            }
            codec::RTM_GETNEIGH if is_dump => {
                if dump.is_some() {
                    return Reply::Error(-EBUSY);
                }
                let (index, proxy) = match codec::neighbor_dump(payload) {
                    Ok(query) => query,
                    Err(errno) => return Reply::Error(-errno),
                };
                let rows = if proxy || !matches!(family, AF_UNSPEC | AF_INET | AF_INET6) {
                    Ok(Rows::Empty)
                } else {
                    match neighbor_snapshot(scope, index) {
                        Ok(rows) => codec::validate_neighbors(&rows).map(|()| Rows::Neighbors {
                            rows,
                            interface: index,
                            family,
                        }),
                        Err(SocketError::NoDevice) => Ok(Rows::Empty),
                        Err(error) => Err(socket_errno(error)),
                    }
                };
                match rows {
                    Ok(rows) => {
                        *dump = Some(new_dump(rows, header.sequence));
                        Reply::None
                    }
                    Err(error) => Reply::Error(-error),
                }
            }
            codec::RTM_GETNEIGH => self.single_neighbor(scope, header, payload, port),
            codec::RTM_NEWNEIGH | codec::RTM_DELNEIGH | codec::RTM_NEWRULE | codec::RTM_DELRULE => {
                Reply::Error(-change::EPERM)
            }
            // 默认域由原生安装事务管理。独立 NEWNET 仍按 owning UserNS 检查 CAP_NET_ADMIN。
            codec::RTM_NEWLINK
            | codec::RTM_DELLINK
            | codec::RTM_SETLINK
            | codec::RTM_NEWADDR
            | codec::RTM_DELADDR
            | codec::RTM_NEWROUTE
            | codec::RTM_DELROUTE
                if scope.is_managed() || !net_admin =>
            {
                Reply::Error(-change::EPERM)
            }
            codec::RTM_NEWLINK | codec::RTM_SETLINK => {
                changed(change::link(scope, header.flags, payload))
            }
            codec::RTM_NEWADDR => changed(change::address_new(scope, header.flags, payload)),
            codec::RTM_DELADDR => changed(change::address_delete(scope, payload)),
            codec::RTM_NEWROUTE => changed(change::route(scope, false, header.flags, payload)),
            codec::RTM_DELROUTE => changed(change::route(scope, true, header.flags, payload)),
            // 非 dump 的 GETADDR/GETRULE，以及删除设备等其他类型尚未实现。
            _ => Reply::Error(-EOPNOTSUPP),
        }
    }

    /// 查找指定接口/地址的真实 ARP/ND 条目；未知设备与未命中分别返回 ENODEV/ENOENT。
    fn single_neighbor(
        &self,
        scope: &super::NetNamespace,
        header: &codec::RequestHeader,
        payload: &[u8],
        port: u32,
    ) -> Reply {
        let (index, address, proxy) = match codec::neighbor_get(payload) {
            Ok(query) => query,
            Err(errno) => return Reply::Error(-errno),
        };
        let snapshot = match neighbor_snapshot(scope, index) {
            Ok(rows) => rows,
            Err(error) => return Reply::Error(-socket_errno(error)),
        };
        if let Err(errno) = codec::validate_neighbors(&snapshot) {
            return Reply::Error(-errno);
        }
        let row = (0..snapshot.len())
            .filter_map(|index| snapshot.get(index))
            .find(|row| !proxy && row.address == address);
        let Some(row) = row else {
            return Reply::Error(-2);
        };
        let (bytes, length) = codec::neighbor(row, 0, header.sequence, port);
        let mut datagram = Vec::new();
        if datagram.try_reserve_exact(length).is_err() {
            return Reply::Error(-ENOMEM);
        }
        datagram.extend_from_slice(&bytes[..length]);
        Reply::Datagram(datagram)
    }

    /// 非 dump 的 `RTM_GETLINK`：按 `ifi_index` 或 `IFLA_IFNAME` 查询一个设备。
    fn single_link(
        &self,
        scope: &super::NetNamespace,
        header: &codec::RequestHeader,
        payload: &[u8],
        port: u32,
    ) -> Reply {
        // ifinfomsg：family、pad、type、index(i32)、flags、change，共 16 字节。
        if payload.len() < 16 {
            return Reply::Error(-EINVAL);
        }
        let index = i32::from_ne_bytes(payload[4..8].try_into().expect("ifinfomsg index"));
        let name = codec::find_attribute(payload, 16, codec::IFLA_IFNAME_ATTRIBUTE);
        let rows = match collect_links(scope) {
            Ok(rows) => rows,
            Err(error) => return Reply::Error(-socket_errno(error)),
        };
        let selected = if index > 0 {
            rows.iter()
                .find(|(snapshot, _)| snapshot.index == index as u32)
        } else if let Some(name) = name {
            let name = name.split(|byte| *byte == 0).next().unwrap_or(name);
            rows.iter().find(|(snapshot, _)| {
                let end = snapshot.name.iter().position(|b| *b == 0).unwrap_or(16);
                &snapshot.name[..end] == name
            })
        } else {
            return Reply::Error(-EINVAL);
        };
        let Some((snapshot, statistics)) = selected else {
            return Reply::Error(-ENODEV);
        };
        let (bytes, length) = codec::link(snapshot, statistics, 0, header.sequence, port);
        let mut datagram = Vec::new();
        if datagram.try_reserve_exact(length).is_err() {
            return Reply::Error(-ENOMEM);
        }
        datagram.extend_from_slice(&bytes[..length]);
        Reply::Datagram(datagram)
    }

    /// 把一次 send 产生的回复一次性发表到 state，并在变为可读时唤醒等待者。
    fn install(&self, control: VecDeque<Vec<u8>>, dump: Option<Dump>) {
        let notify = {
            let mut guard = self.state.lock();
            let Some(state) = guard.as_mut() else {
                return;
            };
            let was_readable =
                state.pending_error || !state.control.is_empty() || state.dump.is_some();
            if state.control.len() + control.len() > MAX_CONTROL_DATAGRAMS {
                // 接收方没有及时读走：按 netlink overrun 通知一次，丢弃新回复。
                state.pending_error = true;
            } else {
                state.control.extend(control);
                if dump.is_some() && state.dump.is_none() {
                    state.dump = dump;
                }
            }
            !was_readable
                && (state.pending_error || !state.control.is_empty() || state.dump.is_some())
        };
        if notify {
            self.notify_write.signal_readiness();
        }
    }

    pub(super) fn receive(
        &self,
        output: &mut ReceiveBuffer<'_>,
        peek: bool,
    ) -> Result<ReceivedMessage, SocketError> {
        let capacity = output.remaining().clamp(1024, MAX_DATAGRAM);
        let mut chunk: Vec<u8> = Vec::new();
        // 1. 只观察当前不可变回复的存储需求；分配和用户输出准备均不持有 state guard。
        loop {
            let required = {
                let guard = self.state.lock();
                let state = guard.as_ref().ok_or(SocketError::NotConnected)?;
                reply_capacity(state, capacity)
            };
            chunk
                .try_reserve_exact(required)
                .map_err(|_| SocketError::NoMemory)?;
            output
                .prepare_append(required)
                .map_err(|()| SocketError::NoMemory)?;
            let mut retired_control = None;
            let mut retired_dump = None;
            {
                let mut guard = self.state.lock();
                let state = guard.as_mut().ok_or(SocketError::NotConnected)?;
                // 2. 另一线程可能换入更大的回复；重做外部分配，不推进游标或摘除数据报。
                let needed = reply_capacity(state, capacity);
                if needed > chunk.capacity()
                    || needed.min(output.remaining()) > output.prepared_remaining()
                {
                    continue;
                }
                if state.pending_error {
                    state.pending_error = false;
                    return Err(SocketError::NoBuffer);
                }
                let port = state.address.map_or(0, |address| address.port_id);
                if let Some(front) = state.control.front() {
                    chunk.extend_from_slice(front);
                    if !peek {
                        retired_control = state.control.pop_front();
                    }
                } else if let Some(dump) = state.dump.as_mut() {
                    let finished = fill_chunk(dump, port, capacity, &mut chunk, peek);
                    if finished && !peek {
                        retired_dump = state.dump.take();
                    }
                } else {
                    return Err(SocketError::Again);
                }
            }
            // 3. 所有可失败准备已完成；guard 外释放退休对象，再无分配地复制已初始化正文。
            drop((retired_control, retired_dump));
            break;
        }
        let full_length = chunk.len();
        let count = output.append(&chunk);
        Ok(ReceivedMessage {
            count,
            full_length,
            // 回复来自内核：port 0、无 group。
            source: Some(SocketAddress::Netlink(NetlinkAddress {
                port_id: 0,
                groups: 0,
            })),
            internet_error: None,
            internet_packet_info: None,
            packet_information: None,
            rights: None,
            credentials: None,
        })
    }

    fn projection(state: Option<&RouteSocketState>) -> SocketPollState {
        let Some(state) = state else {
            return SocketPollState::closed();
        };
        SocketPollState {
            readable: !state.control.is_empty() || state.dump.is_some() || state.pending_error,
            writable: true,
            hangup: false,
            read_closed: false,
            error: state.pending_error,
        }
    }

    pub(super) fn poll_state(&self) -> SocketPollState {
        Self::projection(self.state.lock().as_ref())
    }

    pub(super) fn try_poll_state(&self) -> Option<SocketPollState> {
        Some(Self::projection(self.state.try_lock()?.as_ref()))
    }

    pub(super) fn take_error(&self) -> Option<SocketError> {
        let mut state = self.state.lock();
        let state = state.as_mut()?;
        core::mem::take(&mut state.pending_error).then_some(SocketError::NoBuffer)
    }

    pub(super) fn set_buffer_limit(
        &self,
        direction: SocketIoDirection,
        value: i32,
        forced: bool,
    ) -> Result<(), SocketError> {
        let maximum = match direction {
            SocketIoDirection::Receive => CoreBufferLimit::ReceiveMaximum,
            SocketIoDirection::Send => CoreBufferLimit::SendMaximum,
        };
        let requested = if forced {
            value.max(0) as usize
        } else {
            (value as u32 as usize).min(maximum.value() as usize)
        }
        .min(i32::MAX as usize / 2);
        let mut guard = self.state.lock();
        let state = guard.as_mut().ok_or(SocketError::NotConnected)?;
        // Linux 记录的是请求值的两倍（含内核开销），并有最小值。
        let stored = (requested * 2).max(2048);
        match direction {
            SocketIoDirection::Receive => state.receive_limit = stored,
            SocketIoDirection::Send => state.send_limit = stored,
        }
        Ok(())
    }

    pub(super) fn buffer_limit(&self, direction: SocketIoDirection) -> Result<i32, SocketError> {
        let guard = self.state.lock();
        let state = guard.as_ref().ok_or(SocketError::NotConnected)?;
        Ok(match direction {
            SocketIoDirection::Receive => state.receive_limit,
            SocketIoDirection::Send => state.send_limit,
        } as i32)
    }

    pub(super) fn readiness_generation(&self) -> u64 {
        self.notify_read
            .pipe()
            .readiness_generation(PipeDirection::Read)
    }

    pub(super) fn wait_source(&self) -> Arc<Pipe> {
        self.notify_read.pipe()
    }

    pub(super) fn consume_wait_notification(&self) {
        self.notify_read.drain_readiness();
    }

    /// 唯一 fput：先退休 state，再释放 port 索引，二者不嵌套持锁。
    pub(super) fn close_description(&self) {
        let state = self.state.lock().take();
        let port = state
            .as_ref()
            .and_then(|state| state.address)
            .and_then(|address| {
                let key = (
                    PROTOCOL_ROUTE,
                    self.memory.namespace_ref().id(),
                    address.port_id,
                );
                kobject::with_ports(|ports| ports.take_entry(&key))
            });
        drop((port, state));
        self.notify_read.close();
        self.notify_write.close();
    }
}

/// 处理一条请求的结果。
enum Reply {
    None,
    Datagram(Vec<u8>),
    Error(i32),
}

/// 修改请求的结果：成功没有回复数据（需要时由 `NLM_F_ACK` 补确认），失败是负 errno。
fn changed(result: Result<(), i32>) -> Reply {
    match result {
        Ok(()) => Reply::None,
        Err(errno) => Reply::Error(-errno),
    }
}

fn new_dump(rows: Rows, sequence: u32) -> Dump {
    Dump {
        rows,
        next: 0,
        sequence,
    }
}

fn collect_links(
    scope: &super::NetNamespace,
) -> Result<Vec<(InterfaceSnapshot, NetworkStatistics)>, SocketError> {
    let interfaces = interface_snapshots(scope)?;
    let mut statistics = network_snapshot(scope)?;
    statistics.sort_unstable_by_key(|row| row.index);
    let mut rows = Vec::new();
    rows.try_reserve_exact(interfaces.len())
        .map_err(|_| SocketError::NoMemory)?;
    for interface in interfaces {
        // 两份快照之间接口可能撤下或复用名称。只配对同一身份，避免把新设备的流量归给旧设备。
        if let Ok(position) = statistics.binary_search_by_key(&interface.index, |row| row.index) {
            rows.push((interface, statistics[position].statistics));
        }
    }
    Ok(rows)
}

/// 当前待交付回复的容量上界；只读冻结布局，不在 guard 内分配。
fn reply_capacity(state: &RouteSocketState, window: usize) -> usize {
    window.max(state.control.front().map_or_else(
        || {
            state
                .dump
                .as_ref()
                .map_or(0, |dump| dump.rows.maximum_message_size())
        },
        |front| front.len(),
    ))
}

/// 把 dump 的下一个数据报写进 `chunk`；`peek` 时不推进游标。
///
/// # Returns
///
/// 本数据报之后 dump 是否已经结束（`NLMSG_DONE` 已写入）。
fn fill_chunk(dump: &mut Dump, port: u32, limit: usize, chunk: &mut Vec<u8>, peek: bool) -> bool {
    let mut next = dump.next;
    let total = dump.rows.len();
    // 第一条消息总是写入，哪怕比接收缓冲大（被截断）；其后的消息必须整条放得下。
    let push = |encoded: ([u8; 1024], usize), chunk: &mut Vec<u8>| -> bool {
        let (bytes, length) = encoded;
        if !chunk.is_empty() && chunk.len() + length > limit {
            return false;
        }
        chunk.extend_from_slice(&bytes[..length]);
        true
    };
    while next < total {
        let encoded = match &dump.rows {
            Rows::Empty => None,
            Rows::Links(rows) => {
                let (snapshot, statistics) = &rows[next];
                Some(codec::link(
                    snapshot,
                    statistics,
                    codec::NLM_F_MULTI,
                    dump.sequence,
                    port,
                ))
            }
            Rows::Addresses { family, rows } => {
                let row = rows.get(next).expect("frozen address row");
                if *family == AF_UNSPEC || (*family == AF_INET) == row.address.is_ipv4() {
                    Some(codec::address(row, dump.sequence, port))
                } else {
                    None
                }
            }
            Rows::Rules(rows) => Some(codec::rule(
                rows.get(next).expect("frozen rule row"),
                dump.sequence,
                port,
            )),
            Rows::Neighbors {
                rows,
                interface,
                family,
            } => {
                let row = rows.get(next).expect("frozen neighbor row");
                if *family == AF_UNSPEC || (*family == AF_INET) == row.address.is_ipv4() {
                    Some(codec::neighbor(
                        row,
                        codec::NLM_F_MULTI
                            | if *interface != 0 {
                                codec::NLM_F_DUMP_FILTERED
                            } else {
                                0
                            },
                        dump.sequence,
                        port,
                    ))
                } else {
                    None
                }
            }
            Rows::Routes { ipv4, ipv6 } => {
                let count4 = ipv4.as_ref().map_or(0, |rows| rows.len());
                let appended = if next < count4 {
                    ipv4.as_ref().expect("IPv4 dump range").append(
                        next,
                        dump.sequence,
                        port,
                        limit,
                        chunk,
                    )
                } else {
                    ipv6.as_ref().expect("IPv6 dump range").append(
                        next - count4,
                        dump.sequence,
                        port,
                        limit,
                        chunk,
                    )
                };
                if !appended {
                    break;
                }
                next += 1;
                continue;
            }
        };
        // 当前地址族没有地址的接口直接跳过，仍保留同一冻结快照和游标。
        let Some(encoded) = encoded else {
            next += 1;
            continue;
        };
        if !push(encoded, chunk) {
            break;
        }
        next += 1;
    }
    let finished = next >= total && push(codec::done(dump.sequence, port), chunk);
    if !peek {
        dump.next = next;
    }
    finished
}
