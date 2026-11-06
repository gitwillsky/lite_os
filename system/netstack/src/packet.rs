use crate::sync::Mutex;
use alloc::{
    collections::VecDeque,
    sync::{Arc, Weak},
    vec::Vec,
};
use native_runtime::Event;
use native_runtime::native_abi::signals;
use native_runtime::sync::Once;

use crate::{namespace::Namespace, receive::ReceiveBuffer};
use device_model::network::{NetworkError, NetworkTransmit};
use fallible_tree::FallibleMap;
use network_protocol::{PacketAddress, PacketKind, SocketError, SocketPollState};

const ETH_HEADER_LENGTH: usize = 14;
const ARPHRD_ETHER: u16 = 1;
const ETH_P_ALL: u16 = 0x0003;
const PACKET_HOST: u8 = 0;
const PACKET_BROADCAST: u8 = 1;
const PACKET_MULTICAST: u8 = 2;
const PACKET_OTHERHOST: u8 = 3;
const PACKET_OUTGOING: u8 = 4;
const RECEIVE_QUEUE_LIMIT: usize = 64;

struct SharedPacket {
    link_header: [u8; ETH_HEADER_LENGTH],
    payload: Vec<u8>,
    source: PacketAddress,
}

struct EndpointState {
    endpoint: Weak<PacketSocket>,
    net_id: u64,
    pending_error: Option<SocketError>,
    protocol: u16,
    interface_index: i32,
    queue: VecDeque<Arc<SharedPacket>>,
    // empty → readable edge 已发布到 queue、尚未在 registry lock 外通知；缺失时长期
    // readable queue 会在每次网络 poll 重复唤醒 waiter。
    notification_pending: bool,
}

struct PacketRegistry {
    endpoints: FallibleMap<usize, EndpointState>,
    next_id: usize,
}

// OWNER: 目录唯一持实际AF_PACKET binding/queue与Weak endpoint；不持NIC或强NET引用。
// 每endpoint捕获同一SocketMemory.NET，目录net_id仅为不可复用membership key。实际设备tap
// 在L3接受之前发布；不能把队列复制进INET或从当前Task/initial域重建归属。
static PACKET_REGISTRY: Once<Mutex<PacketRegistry>> = Once::new();

fn registry() -> Result<&'static Mutex<PacketRegistry>, SocketError> {
    PACKET_REGISTRY.get().ok_or(SocketError::NetworkUnreachable)
}

/// 初始化唯一AF_PACKET membership目录；设备只属于捕获NET的实际interface owner。
///
/// # Returns
///
/// 与物理设备发现无关，新NET只有lo也可创建packet socket。
///
/// # Errors
///
/// 无返回错误；重复调用由 Once 保持幂等。
pub(crate) fn init() {
    PACKET_REGISTRY.call_once(|| {
        Mutex::new(PacketRegistry {
            endpoints: FallibleMap::new(),
            next_id: 1,
        })
    });
}

/// 原生AF_PACKET RAW/DGRAM endpoint，共用同一实际packet/链路事实。
pub(crate) struct PacketSocket {
    id: usize,
    kind: PacketKind,
    namespace: Arc<Namespace>,
    // 使用者等待的就绪 Event：队列变为可读、发送槽恢复或出现错误时置位 `USER_0`。
    readiness: Arc<Event>,
}

impl PacketSocket {
    /// 创建绑定指定 network-byte-order protocol 的 packet endpoint。
    ///
    /// # Parameters
    ///
    /// - `kind`: 链路头处理方式（RAW 或 DGRAM）。
    /// - `protocol`: `socket(2)` 传入的 network-byte-order EtherType。
    /// - `readiness`: endpoint 独占的就绪 Event。
    /// - `namespace`: endpoint 所在的域。
    ///
    /// # Returns
    ///
    /// 已注册且可被 RX tap 发现的 endpoint Arc。
    ///
    /// # Errors
    ///
    /// protocol不能表示为EtherType、registry未初始化、准备OOM或id耗尽时返回错误。
    pub(crate) fn new(
        kind: PacketKind,
        protocol: u16,
        readiness: Arc<Event>,
        namespace: Arc<Namespace>,
    ) -> Result<Arc<Self>, SocketError> {
        let owner = registry()?;
        // 固定 admission 队列容量在 spin guard 外一次准备，RX publication 不再分配。
        let mut queue = VecDeque::new();
        queue
            .try_reserve_exact(RECEIVE_QUEUE_LIMIT)
            .map_err(|_| SocketError::NoMemory)?;
        // 1. Arc 与 registry node storage 在取得 registry guard 前准备；OOM 不执行伪 close。
        let mut allocation = Arc::<Self>::try_new_uninit().map_err(|_| SocketError::NoMemory)?;
        let node = FallibleMap::<usize, EndpointState>::try_reserve_node()
            .map_err(|_| SocketError::NoMemory)?;
        let id = {
            let mut registry = owner.lock();
            let id = registry.next_id;
            registry.next_id = id.checked_add(1).ok_or(SocketError::NoMemory)?;
            id
        };
        Arc::get_mut(&mut allocation)
            .expect("unpublished packet endpoint is unique")
            .write(Self {
                id,
                kind,
                namespace: namespace.clone(),
                readiness,
            });
        // SAFETY: 尚未 clone/Weak 的唯一 storage 已完整写入 Self；之后 admission 不分配、不失败。
        let endpoint = unsafe { allocation.assume_init() };
        let prepared = node.fill(
            id,
            EndpointState {
                endpoint: Arc::downgrade(&endpoint),
                net_id: namespace.id(),
                pending_error: None,
                protocol,
                interface_index: 0,
                queue,
                notification_pending: false,
            },
        );
        owner.lock().endpoints.commit_vacant(prepared);
        Ok(endpoint)
    }

    /// 原生packet_do_bind：实际NET的设备或ifindex0，非零protocol更新、允许重新绑定。
    ///
    /// # Parameters
    ///
    /// - `address`: userspace `sockaddr_ll` 的完整语义值。
    ///
    /// # Returns
    ///
    /// 实际binding已发表，设备DOWN仍成功并保存一次ENETDOWN。
    ///
    /// # Errors
    ///
    /// 未找到实际接口为ENODEV；已关闭endpoint为NotConnected。
    pub(crate) fn bind(&self, address: PacketAddress) -> Result<(), SocketError> {
        // 实际OFD scope查名字/index；设备查找在packet spin guard外，避免stack→tap反向锁序。
        let up = if address.interface_index != 0 {
            if address.interface_index < 0 {
                return Err(SocketError::NoDevice);
            }
            Some(
                crate::inet::packet_interface(&self.namespace, address.interface_index as u32)?
                    .0
                    .up,
            )
        } else {
            None
        };
        let mut registry = registry()?.lock();
        let state = registry
            .endpoints
            .get_mut(&self.id)
            .ok_or(SocketError::NotConnected)?;
        if address.protocol != 0 {
            state.protocol = address.protocol;
        }
        state.interface_index = address.interface_index;
        if up == Some(false) {
            state.pending_error = Some(SocketError::NetworkDown);
        }
        drop(registry);
        if up == Some(false) {
            self.notify();
        }
        Ok(())
    }

    /// 返回 endpoint 权威 `sockaddr_ll` binding。
    ///
    /// # Returns
    ///
    /// 未 bind 时 ifindex 为零，其余字段保持 Linux packet socket 形状。
    ///
    /// # Errors
    ///
    /// registry 或 endpoint 已消失时返回错误。
    pub(crate) fn address(&self) -> Result<PacketAddress, SocketError> {
        let (protocol, interface_index) = {
            let registry = registry()?.lock();
            let state = registry
                .endpoints
                .get(&self.id)
                .ok_or(SocketError::NotConnected)?;
            (state.protocol, state.interface_index)
        };
        let (hardware_type, address_length, address) = if interface_index != 0 {
            let (interface, _) =
                crate::inet::packet_interface(&self.namespace, interface_index as u32)?;
            (
                if interface.loopback {
                    772
                } else {
                    ARPHRD_ETHER
                },
                6,
                padded_address(interface.mac),
            )
        } else {
            (0, 0, [0; 8])
        };
        Ok(PacketAddress {
            protocol,
            interface_index,
            hardware_type,
            packet_type: PACKET_HOST,
            address_length,
            address,
        })
    }

    /// 按实际RAW/DGRAM角色提交link packet，不改变endpoint binding。
    ///
    /// # Parameters
    ///
    /// - `input`: RAW为完整link frame，DGRAM为link payload。
    /// - `target`: 显式实际NET出口/protocol/address；RAW或lo可使用现有binding。
    ///
    /// # Returns
    ///
    /// 成功提交的 L3 byte count。
    ///
    /// # Errors
    ///
    /// target/MTU 无效或 adapter 发送失败时返回标准 socket error。
    pub(crate) fn send_to(
        &self,
        input: &[u8],
        target: Option<PacketAddress>,
    ) -> Result<usize, SocketError> {
        let binding = {
            let registry = registry()?.lock();
            let state = registry
                .endpoints
                .get(&self.id)
                .ok_or(SocketError::NotConnected)?;
            (state.interface_index, state.protocol)
        };
        let explicit = target.is_some();
        let target = target.unwrap_or(PacketAddress {
            protocol: binding.1,
            interface_index: binding.0,
            hardware_type: 0,
            packet_type: 0,
            address_length: 6,
            address: [0; 8],
        });
        if target.interface_index <= 0 {
            return Err(SocketError::DeviceNotAvailable);
        }
        let (interface, device) =
            crate::inet::packet_interface(&self.namespace, target.interface_index as u32).map_err(
                |error| {
                    if error == SocketError::NoDevice {
                        SocketError::DeviceNotAvailable
                    } else {
                        error
                    }
                },
            )?;
        if !interface.up {
            return Err(SocketError::NetworkDown);
        }
        let raw = self.kind == PacketKind::Raw;
        if input.len() > interface.mtu as usize + if raw { ETH_HEADER_LENGTH } else { 0 } {
            return Err(SocketError::MessageTooLarge);
        }
        if raw && input.len() < ETH_HEADER_LENGTH {
            return Err(SocketError::Invalid);
        }
        if target.address_length > 8 {
            return Err(SocketError::Invalid);
        }
        // Linuxpacket_snd的显式sockaddr可覆盖bound device/protocol；不重新绑定endpoint。
        let mut link_header = [0u8; ETH_HEADER_LENGTH];
        let payload = if raw {
            link_header.copy_from_slice(&input[..ETH_HEADER_LENGTH]);
            &input[ETH_HEADER_LENGTH..]
        } else {
            link_header[..6].copy_from_slice(&target.address[..6]);
            link_header[6..12].copy_from_slice(&interface.mac);
            link_header[12..14].copy_from_slice(&u16::from_be(target.protocol).to_be_bytes());
            input
        };
        if interface.loopback {
            return crate::inet::inject_loopback(&self.namespace, link_header, payload, self.id)
                .map(|()| input.len());
        }
        if !raw && !explicit {
            return Err(SocketError::Invalid);
        }
        let device = device.expect("Ethernet interface owns its NIC");
        let transmit = NetworkTransmit::reserve(device).map_err(network_error)?;
        let mut frame = Vec::new();
        frame
            .try_reserve_exact(ETH_HEADER_LENGTH + payload.len())
            .map_err(|_| SocketError::NoMemory)?;
        frame.extend_from_slice(&link_header);
        frame.extend_from_slice(payload);
        deliver_outgoing(
            self.namespace.id(),
            interface.index,
            ARPHRD_ETHER,
            link_header,
            payload,
            u16::from_be(target.protocol),
            Some(self.id),
        );
        transmit.submit(&frame).map_err(network_error)?;
        Ok(input.len())
    }

    /// RAW保留实际link header，DGRAM剥除header；peek与原始长度沿同一queue。
    ///
    /// # Parameters
    ///
    /// - `output`: kernel-owned 输出 buffer。
    /// - `peek`: true 时不消费 queue head。
    ///
    /// # Returns
    ///
    /// copied/full length 与 source `sockaddr_ll`。
    ///
    /// # Errors
    ///
    /// queue 为空返回 Again；registry 状态损坏返回 NotConnected。
    pub(crate) fn receive(
        &self,
        output: &mut ReceiveBuffer,
        peek: bool,
    ) -> Result<(usize, usize, PacketAddress), SocketError> {
        let mut registry = registry()?.lock();
        let state = registry
            .endpoints
            .get_mut(&self.id)
            .ok_or(SocketError::NotConnected)?;
        let packet = if peek {
            state.queue.front().ok_or(SocketError::Again)?.clone()
        } else {
            state.queue.pop_front().ok_or(SocketError::Again)?
        };
        drop(registry);
        // 不在 registry guard 内复制 payload 或退休最后一个 SharedPacket owner。
        let include_header = self.kind == PacketKind::Raw;
        let full_length = packet.payload.len() + if include_header { ETH_HEADER_LENGTH } else { 0 };
        let mut count = if include_header {
            output.append(&packet.link_header)
        } else {
            0
        };
        count += output.append(&packet.payload);
        Ok((count, full_length, packet.source))
    }

    /// 从唯一 receive queue 投影 packet endpoint readiness。
    ///
    /// # Returns
    ///
    /// queue 非空时 readable，TX pool 尚有 free slot 时 writable。
    ///
    /// # Errors
    ///
    /// registry 消失投影为 not-readable；发送时再返回具体错误。
    pub(crate) fn poll_state(&self) -> SocketPollState {
        self.try_poll_state().unwrap_or_else(SocketPollState::error)
    }
    /// deferred观察不得在packet spin guard下等待protocol owner。
    fn try_poll_state(&self) -> Option<SocketPollState> {
        let owner = registry().ok()?;
        let registry = owner.try_lock()?;
        let Some(state) = registry.endpoints.get(&self.id) else {
            return Some(SocketPollState::closed());
        };
        let (readable, index, error) = (
            !state.queue.is_empty(),
            state.interface_index,
            state.pending_error.is_some(),
        );
        drop(registry);
        Some(SocketPollState {
            readable,
            writable: crate::inet::packet_transmit_available(&self.namespace, index)?,
            hangup: false,
            read_closed: false,
            error,
        })
    }
    /// 唯一packet pending error，SO_ERROR读取后消费一次。
    pub(crate) fn take_error(&self) -> Option<SocketError> {
        registry()
            .ok()?
            .lock()
            .endpoints
            .get_mut(&self.id)?
            .pending_error
            .take()
    }

    /// 在 registry lock 已释放后发布一次 level-triggered readable 通知。
    ///
    /// # Errors
    ///
    /// 置位失败（Event 已关闭）时忽略：使用者已不再等待。
    pub(crate) fn notify(&self) {
        let _ = self.readiness.signal(0, signals::USER_0);
    }

    /// actual registry membership 是唯一 admission；队列及 Weak node 在 guard 外退休。
    pub(crate) fn close_description(&self) {
        let retired = PACKET_REGISTRY
            .get()
            .and_then(|registry| registry.lock().endpoints.remove(&self.id));
        drop(retired);
    }
}

/// 在 smoltcp 解析前将一个 Ethernet frame 镜像给匹配的 packet endpoints。
///
/// # Parameters
///
/// - `frame`: 包含 Ethernet header 的完整 RX frame。
///
/// # Returns
///
/// 本轮从 empty 转为 readable、且需在 NetworkStack 解锁后唤醒的 endpoints。
///
/// # Errors
///
/// 无匹配NET/interface/protocol或队列已满时跳过，不改变L3 ingress。
pub(crate) fn deliver(net_id: u64, index: u32, mac: [u8; 6], frame: &[u8]) {
    if frame.len() < ETH_HEADER_LENGTH {
        return;
    }
    let protocol = u16::from_be_bytes([frame[12], frame[13]]);
    let source = PacketAddress {
        protocol: protocol.to_be(),
        interface_index: index as i32,
        hardware_type: ARPHRD_ETHER,
        packet_type: packet_type(&frame[..6], mac),
        address_length: 6,
        address: padded_address(frame[6..12].try_into().unwrap()),
    };
    deliver_packet(
        net_id,
        source,
        frame[..ETH_HEADER_LENGTH].try_into().unwrap(),
        &frame[ETH_HEADER_LENGTH..],
        None,
    );
}
/// 实际IP-medium lo tap沿原生sockaddr_ll发布，不伪造Ethernet/ARP协议设备。
pub(crate) fn deliver_loopback(net_id: u64, header: [u8; ETH_HEADER_LENGTH], packet: &[u8]) {
    deliver_packet(
        net_id,
        PacketAddress {
            protocol: u16::from_be_bytes([header[12], header[13]]).to_be(),
            interface_index: 1,
            hardware_type: 772,
            packet_type: packet_type(&header[..6], [0; 6]),
            address_length: 6,
            address: padded_address(header[6..12].try_into().unwrap()),
        },
        header,
        packet,
        None,
    );
}
/// 实际NIC提交的L2报文；排除发起该packet socket，其他同域binding按原生OUTGOING收取。
pub(crate) fn deliver_outgoing(
    net_id: u64,
    index: u32,
    hardware_type: u16,
    header: [u8; ETH_HEADER_LENGTH],
    payload: &[u8],
    protocol: u16,
    sender: Option<usize>,
) {
    let source = PacketAddress {
        protocol: protocol.to_be(),
        interface_index: index as i32,
        hardware_type,
        packet_type: PACKET_OUTGOING,
        address_length: 6,
        address: padded_address(header[6..12].try_into().unwrap()),
    };
    deliver_packet(net_id, source, header, payload, sender);
}

fn matches(state: &EndpointState, net_id: u64, source: PacketAddress) -> bool {
    state.net_id == net_id
        && (state.interface_index == 0 || state.interface_index == source.interface_index)
        && state.protocol != 0
        && (u16::from_be(state.protocol) == ETH_P_ALL || state.protocol == source.protocol)
        && state.queue.len() < RECEIVE_QUEUE_LIMIT
}
fn deliver_packet(
    net_id: u64,
    source: PacketAddress,
    header: [u8; ETH_HEADER_LENGTH],
    packet: &[u8],
    sender: Option<usize>,
) {
    let Some(registry) = PACKET_REGISTRY.get() else {
        return;
    };
    if !registry
        .lock()
        .endpoints
        .iter()
        .any(|(&id, state)| sender != Some(id) && matches(state, net_id, source))
    {
        return;
    }
    let mut bytes = Vec::new();
    if bytes.try_reserve_exact(packet.len()).is_err() {
        return;
    }
    bytes.extend_from_slice(packet);
    let Ok(payload) = Arc::try_new(SharedPacket {
        link_header: header,
        payload: bytes,
        source,
    }) else {
        return;
    };
    let mut registry = registry.lock();
    registry.endpoints.for_each_mut(|&id, state| {
        if sender == Some(id) || !matches(state, net_id, source) {
            return;
        }
        let was_empty = state.queue.is_empty();
        assert!(
            state.queue.capacity() >= RECEIVE_QUEUE_LIMIT,
            "packet queue admission lost capacity"
        );
        state.queue.push_back(payload.clone());
        state.notification_pending |= was_empty;
    });
}

/// TX capacity从零转为非零，只向同一NET/实际interface的packet bindings发表edge。
///
/// # Returns
///
/// 无返回值；没有 live endpoint 时为空操作。
///
/// # Errors
///
/// 不分配；实际 Pipe notification 由 NetworkStack lock 外的统一 drain 完成。
pub(crate) fn publish_transmit_ready(net_id: u64, index: u32) {
    let Some(registry) = PACKET_REGISTRY.get() else {
        return;
    };
    registry.lock().endpoints.for_each_mut(|_, state| {
        if state.net_id == net_id
            && (state.interface_index == 0 || state.interface_index == index as i32)
        {
            state.notification_pending = true;
        }
    });
}

/// NETDEV_UNREGISTER：绑定该接口的 endpoint 解除绑定（ifindex -1）并报告 ENETDOWN，与 Linux
/// `packet_notifier` 一致；edge 由随后的统一 drain 在 registry lock 外发布。
pub(crate) fn publish_interface_removed(net_id: u64, index: u32) {
    let Some(registry) = PACKET_REGISTRY.get() else {
        return;
    };
    registry.lock().endpoints.for_each_mut(|_, state| {
        if state.net_id == net_id && state.interface_index == index as i32 {
            state.interface_index = -1;
            state.pending_error = Some(SocketError::NetworkDown);
            state.notification_pending = true;
        }
    });
}

/// 按 endpoint ID 取出下一个待发布的 packet readable edge。
///
/// # Parameters
///
/// - `after`: exclusive ID cursor。
///
/// # Returns
///
/// live 且拥有 pending edge 的最小后继 endpoint；返回前原子消费 edge。
pub(crate) fn take_pending_notification(after: usize) -> Option<(usize, Arc<PacketSocket>)> {
    let mut registry = PACKET_REGISTRY.get()?.lock();
    let (id, endpoint) = registry
        .endpoints
        .iter_after(&after)
        .find_map(|(&id, state)| {
            state
                .notification_pending
                .then(|| state.endpoint.upgrade().map(|endpoint| (id, endpoint)))
                .flatten()
        })?;
    registry
        .endpoints
        .get_mut(&id)
        .expect("selected packet endpoint disappeared")
        .notification_pending = false;
    Some((id, endpoint))
}

fn padded_address(address: [u8; 6]) -> [u8; 8] {
    let mut padded = [0; 8];
    padded[..6].copy_from_slice(&address);
    padded
}

fn packet_type(destination: &[u8], own_mac: [u8; 6]) -> u8 {
    if destination == [0xff; 6] {
        PACKET_BROADCAST
    } else if destination[0] & 1 != 0 {
        PACKET_MULTICAST
    } else if destination == own_mac {
        PACKET_HOST
    } else {
        PACKET_OTHERHOST
    }
}

fn network_error(error: NetworkError) -> SocketError {
    match error {
        NetworkError::WouldBlock => SocketError::Again,
        NetworkError::FrameTooLarge => SocketError::MessageTooLarge,
        NetworkError::Device => SocketError::Device,
        NetworkError::Disconnected => SocketError::NetworkDown,
    }
}
