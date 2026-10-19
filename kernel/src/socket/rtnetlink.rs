//! `NETLINK_ROUTE` endpoint：`getifaddrs`、`ip link/addr/route` 使用的网络状态查询。
//!
//! # 惰性 dump
//!
//! 请求（`RTM_GETLINK/GETADDR/GETROUTE` + `NLM_F_DUMP`）到达时只冻结一份 interface/FIB 快照和游标；
//! 回复在 `recvmsg` 时按接收缓冲区容量逐个数据报生成，最后以 `NLMSG_DONE` 结束。与 Linux 一样，
//! 回复不预先排队，队列内存有界，不需要接收预算；快照冻结保证一次 dump 内部一致。
//!
//! # 范围
//!
//! 只支持查询：`RTM_GETLINK`（dump 与按 index/name 的单设备查询）、`RTM_GETADDR` 与 `RTM_GETROUTE` 的
//! dump，覆盖 IPv4。修改请求（`NEW/DEL/SET*`）与多播订阅返回 `EOPNOTSUPP`，不伪造成功。

use alloc::{collections::VecDeque, sync::Arc, vec::Vec};
use spin::Mutex;

use crate::{
    drivers::network::NetworkStatistics,
    fallible_tree::FallibleMap,
    ipc::{Pipe, PipeDirection, PipeEnd, ReceiveBuffer},
};

use super::{
    CoreBufferLimit, InterfaceSnapshot, NetlinkAddress, ReceivedMessage, RouteSnapshot,
    SocketAddress, SocketError, SocketIoDirection, SocketMemory, SocketPollState,
    interface_snapshots, kobject, network_snapshot, route_snapshot,
};

mod change;
mod codec;

/// `NETLINK_ROUTE` 的协议号，作为 port 空间 key。
const PROTOCOL_ROUTE: u8 = 0;
/// 单个回复数据报的上限；容量更小的接收缓冲会截断，与 Linux 的 MSG_TRUNC 语义一致。
const MAX_DATAGRAM: usize = 16384;
/// 一次 sendmsg 内允许排队的 ACK/错误数据报个数，超出按 netlink overrun 处理。
const MAX_CONTROL_DATAGRAMS: usize = 64;
const AF_UNSPEC: u8 = 0;
const AF_INET: u8 = 2;
const ENOMEM: i32 = 12;
const ENODEV: i32 = 19;
const EBUSY: i32 = 16;
const EINVAL: i32 = 22;
const EOPNOTSUPP: i32 = 95;

enum Rows {
    Links(Vec<(InterfaceSnapshot, NetworkStatistics)>),
    Addresses(Vec<InterfaceSnapshot>),
    Routes(Vec<RouteSnapshot>),
}

impl Rows {
    fn len(&self) -> usize {
        match self {
            Self::Links(rows) => rows.len(),
            Self::Addresses(rows) => rows.len(),
            Self::Routes(rows) => rows.len(),
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
                    Err(error) => Reply::Error(-errno_for(error)),
                }
            }
            codec::RTM_GETLINK => self.single_link(scope, header, payload, port),
            codec::RTM_GETADDR if is_dump => {
                if dump.is_some() {
                    return Reply::Error(-EBUSY);
                }
                // 只有 IPv4；其他地址族的 dump 是空的，仍以 DONE 结束。
                let rows = if matches!(family, AF_UNSPEC | AF_INET) {
                    interface_snapshots(scope).map(|rows| {
                        rows.into_iter()
                            .filter(|row| row.address.is_some())
                            .collect()
                    })
                } else {
                    Ok(Vec::new())
                };
                match rows {
                    Ok(rows) => {
                        *dump = Some(new_dump(Rows::Addresses(rows), header.sequence));
                        Reply::None
                    }
                    Err(error) => Reply::Error(-errno_for(error)),
                }
            }
            codec::RTM_GETROUTE if is_dump => {
                if dump.is_some() {
                    return Reply::Error(-EBUSY);
                }
                let rows = if matches!(family, AF_UNSPEC | AF_INET) {
                    route_snapshot(scope)
                } else {
                    Ok(Vec::new())
                };
                match rows {
                    Ok(rows) => {
                        *dump = Some(new_dump(Rows::Routes(rows), header.sequence));
                        Reply::None
                    }
                    Err(error) => Reply::Error(-errno_for(error)),
                }
            }
            // 修改请求：先检查 CAP_NET_ADMIN（Linux rtnetlink_rcv_msg 的顺序），再分发。
            codec::RTM_NEWLINK
            | codec::RTM_DELLINK
            | codec::RTM_SETLINK
            | codec::RTM_NEWADDR
            | codec::RTM_DELADDR
            | codec::RTM_NEWROUTE
            | codec::RTM_DELROUTE
                if !net_admin =>
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
            // 非 dump 的 GETADDR/GETROUTE，以及删除设备、规则、邻居等其他类型：本实现不支持。
            _ => Reply::Error(-EOPNOTSUPP),
        }
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
            Err(error) => return Reply::Error(-errno_for(error)),
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
        // 回复缓冲在 guard 外准备：至少容纳一条最大消息，其余按接收缓冲容量封顶。
        let capacity = output.remaining().clamp(1024, MAX_DATAGRAM);
        let mut chunk: Vec<u8> = Vec::new();
        chunk
            .try_reserve_exact(capacity)
            .map_err(|_| SocketError::NoMemory)?;
        // 被摘下的节点/游标在 guard 外释放。
        let mut retired_control = None;
        let mut retired_dump = None;
        {
            let mut guard = self.state.lock();
            let state = guard.as_mut().ok_or(SocketError::NotConnected)?;
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
        drop((retired_control, retired_dump));
        let full_length = chunk.len();
        output
            .prepare_append(full_length)
            .map_err(|()| SocketError::NoMemory)?;
        let count = output.append(&chunk);
        Ok(ReceivedMessage {
            count,
            full_length,
            // 回复来自内核：port 0、无 group。
            source: Some(SocketAddress::Netlink(NetlinkAddress {
                port_id: 0,
                groups: 0,
            })),
            internet_packet_info: None,
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

fn errno_for(error: SocketError) -> i32 {
    match error {
        SocketError::NoMemory => ENOMEM,
        _ => EINVAL,
    }
}

fn collect_links(
    scope: &super::NetNamespace,
) -> Result<Vec<(InterfaceSnapshot, NetworkStatistics)>, SocketError> {
    let interfaces = interface_snapshots(scope)?;
    let statistics = network_snapshot(scope)?;
    let mut rows = Vec::new();
    rows.try_reserve_exact(interfaces.len())
        .map_err(|_| SocketError::NoMemory)?;
    for interface in interfaces {
        let counters = statistics
            .iter()
            .find(|row| row.name == interface.name)
            .map(|row| row.statistics)
            .unwrap_or_default();
        rows.push((interface, counters));
    }
    Ok(rows)
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
            Rows::Addresses(rows) => codec::address(&rows[next], dump.sequence, port),
            Rows::Routes(rows) => Some(codec::route(&rows[next], dump.sequence, port)),
        };
        // 没有地址的行（理论上已被过滤）直接跳过。
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
