use crate::sync::Mutex;
use alloc::{
    collections::VecDeque,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::sync::atomic::{AtomicBool, Ordering};
use native_runtime::Event;
use native_runtime::native_abi::signals;
use packet_wire::{
    buffer::PacketBuffer,
    ethernet::{EthernetFrame, EthernetHeader},
};

use crate::{namespace::Namespace, receive::ReceiveBuffer};
use device_model::network::NetworkError;
use fallible_tree::FallibleMap;
use network_protocol::{
    CaptureRing, PacketAddress, PacketChecksum, PacketInformation, PacketKind, SocketError,
    SocketPollState,
};

const ETH_HEADER_LENGTH: usize = packet_wire::ethernet::HEADER_LENGTH;
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
    checksum: PacketChecksum,
    timestamp_ns: u64,
}

struct EndpointState {
    endpoint: Weak<PacketSocket>,
    pending_error: Option<SocketError>,
    protocol: u16,
    interface_index: i32,
    queue: VecDeque<Arc<SharedPacket>>,
    ring: Option<CaptureRing>,
    observed: u32,
    dropped: u32,
    // empty → readable edge 已发布、尚未在 registry lock 外通知；缺失时长期
    // readable endpoint 会在每次网络 poll 重复唤醒 waiter。
    notification_pending: bool,
}

struct RegistryState {
    endpoints: FallibleMap<usize, EndpointState>,
    next_id: usize,
}

/// 一个网络域的抓包绑定与接收队列。实际设备只借用其所属域的目录。
pub(crate) struct PacketRegistry {
    state: Mutex<RegistryState>,
    // OWNER: state 锁内发表的空表投影。无订阅时，RX/TX 不解析链路头或竞争目录锁。
    active: AtomicBool,
}

impl PacketRegistry {
    /// 创建空目录；仅在所属域创建时分配外层 Arc。
    pub(crate) const fn new() -> Self {
        Self {
            state: Mutex::new(RegistryState {
                endpoints: FallibleMap::new(),
                next_id: 1,
            }),
            active: AtomicBool::new(false),
        }
    }

    /// 当前域是否有订阅帧的 packet socket。
    pub(crate) fn has_endpoints(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }
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
    /// 队列或节点分配失败，或本域 endpoint ID 耗尽。
    pub(crate) fn new(
        kind: PacketKind,
        protocol: u16,
        readiness: Arc<Event>,
        namespace: Arc<Namespace>,
    ) -> Result<Arc<Self>, SocketError> {
        let owner = namespace.packets();
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
            let mut registry = owner.state.lock();
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
                pending_error: None,
                protocol,
                interface_index: 0,
                queue,
                ring: None,
                notification_pending: false,
                observed: 0,
                dropped: 0,
            },
        );
        let mut registry = owner.state.lock();
        let was_empty = registry.endpoints.is_empty();
        registry.endpoints.commit_vacant(prepared);
        if was_empty {
            owner.active.store(true, Ordering::Release);
        }
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
        let commit = |down| {
            let mut registry = self.namespace.packets().state.lock();
            let state = registry
                .endpoints
                .get_mut(&self.id)
                .ok_or(SocketError::NotConnected)?;
            if address.protocol != 0 {
                state.protocol = address.protocol;
            }
            state.interface_index = address.interface_index;
            if down {
                state.pending_error = Some(SocketError::NetworkDown);
            }
            Ok(down)
        };
        let notify = match address.interface_index {
            0 => commit(false)?,
            index if index > 0 => {
                crate::inet::packet_interface(&self.namespace, index as u32, |interface, _| {
                    commit(!interface.up)
                })?
            }
            _ => return Err(SocketError::NoDevice),
        };
        if notify {
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
            let registry = self.namespace.packets().state.lock();
            let state = registry
                .endpoints
                .get(&self.id)
                .ok_or(SocketError::NotConnected)?;
            (state.protocol, state.interface_index)
        };
        let (hardware_type, address_length, address) = if interface_index != 0 {
            let (interface, _) = crate::inet::packet_interface(
                &self.namespace,
                interface_index as u32,
                |interface, device| Ok((interface, device)),
            )?;
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
            let registry = self.namespace.packets().state.lock();
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
        let (interface, device) = crate::inet::packet_interface(
            &self.namespace,
            target.interface_index as u32,
            |interface, device| Ok((interface, device)),
        )
        .map_err(|error| {
            if error == SocketError::NoDevice {
                SocketError::DeviceNotAvailable
            } else {
                error
            }
        })?;
        if !interface.up {
            return Err(SocketError::NetworkDown);
        }
        let raw = self.kind == PacketKind::Raw;
        if input.len() > interface.mtu as usize + if raw { ETH_HEADER_LENGTH } else { 0 } {
            return Err(SocketError::MessageTooLarge);
        }
        if target.address_length > 8 {
            return Err(SocketError::Invalid);
        }
        // Linuxpacket_snd的显式sockaddr可覆盖bound device/protocol；不重新绑定endpoint。
        let mut link_header = [0u8; ETH_HEADER_LENGTH];
        let payload = if raw {
            let frame = EthernetFrame::parse(input).map_err(|_| SocketError::Invalid)?;
            link_header.copy_from_slice(frame.header());
            frame.payload()
        } else {
            EthernetHeader {
                destination: target.address[..6].try_into().expect("fixed link address"),
                source: interface.mac,
                type_or_length: u16::from_be(target.protocol),
                tags: &[],
            }
            .emit(&mut link_header)
            .expect("fixed Ethernet header capacity");
            input
        };
        if !interface.loopback && !raw && !explicit {
            return Err(SocketError::Invalid);
        }
        let Some(device) = device else {
            return crate::inet::inject_virtual_packet(
                &self.namespace,
                interface.index,
                link_header,
                payload,
                self.id,
            )
            .map(|()| input.len());
        };
        let mut transmit = device.reserve_transmit(0).map_err(network_error)?;
        let length = ETH_HEADER_LENGTH + payload.len();
        if length > transmit.capacity() {
            return Err(SocketError::MessageTooLarge);
        }
        // payload 一次复制到最终位置，再消费已预留的头部；加头不移动它，也不另分配 frame。
        let mut frame = PacketBuffer::new(
            &mut transmit.buffer_mut()[..length],
            ETH_HEADER_LENGTH..length,
        )
        .expect("validated transmit extent");
        frame.bytes_mut().copy_from_slice(payload);
        frame
            .prepend(ETH_HEADER_LENGTH)
            .expect("reserved MAC headroom")
            .copy_from_slice(&link_header);
        let pending = deliver_outgoing(
            self.namespace.packets(),
            interface.index,
            ARPHRD_ETHER,
            link_header,
            payload,
            false,
            Some(self.id),
        );
        let result = transmit.submit(length, None, None);
        if pending {
            // 直接发送在域工作线程之外，设备提交失败也要交付已经登记的抓包通知。
            crate::worker::request_poll(self.namespace.id());
        }
        result.map_err(network_error)?;
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
    ) -> Result<(usize, usize, PacketAddress, PacketInformation), SocketError> {
        let mut registry = self.namespace.packets().state.lock();
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
        Ok((
            count,
            full_length,
            packet.source,
            PacketInformation {
                checksum: packet.checksum,
                timestamp_ns: packet.timestamp_ns,
                network_offset: if include_header {
                    ETH_HEADER_LENGTH as u16
                } else {
                    0
                },
            },
        ))
    }

    /// 从当前接收目标及实际出口投影 packet endpoint readiness。
    ///
    /// # Returns
    ///
    /// 接收目标有帧时 readable，TX pool 尚有 free slot 时 writable。
    ///
    /// # Errors
    ///
    /// 无法取得实际状态时投影为 error；短暂的 registry 竞争仅延迟查询。
    pub(crate) fn poll_state(&self) -> SocketPollState {
        self.readiness().unwrap_or_else(SocketPollState::error)
    }

    fn readiness(&self) -> Option<SocketPollState> {
        let registry = self.namespace.packets().state.lock();
        let Some(state) = registry.endpoints.get(&self.id) else {
            return Some(SocketPollState::closed());
        };
        let (readable, index, error) = (
            state
                .ring
                .as_ref()
                .map_or_else(|| !state.queue.is_empty(), CaptureRing::readable),
            state.interface_index,
            state.pending_error.is_some(),
        );
        // 来自 socket 数据请求，可等待 NET。先释放 registry，避免反转 ingress 的加锁顺序。
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
        self.namespace
            .packets()
            .state
            .lock()
            .endpoints
            .get_mut(&self.id)?
            .pending_error
            .take()
    }

    /// 冻结并清零本 endpoint 的抓取及丢弃统计。
    /// # Errors
    /// 目录或 endpoint 已关闭。
    pub(crate) fn take_statistics(&self) -> Result<[u32; 2], SocketError> {
        let mut registry = self.namespace.packets().state.lock();
        let state = registry
            .endpoints
            .get_mut(&self.id)
            .ok_or(SocketError::NotConnected)?;
        Ok([
            core::mem::take(&mut state.observed),
            core::mem::take(&mut state.dropped),
        ])
    }

    /// 切换接收目标，保留队列容量供接收环关闭后复用。
    ///
    /// # Errors
    /// endpoint 已关闭、已有活动环或环的链路头与 socket 类型不匹配。
    pub(crate) fn set_receive_ring(&self, ring: Option<CaptureRing>) -> Result<(), SocketError> {
        let expected_header = if self.kind == PacketKind::Raw {
            ETH_HEADER_LENGTH
        } else {
            0
        };
        if ring
            .as_ref()
            .is_some_and(|ring| ring.link_header_length() != expected_header)
        {
            return Err(SocketError::Invalid);
        }
        let mut retired_packets = [const { None }; RECEIVE_QUEUE_LIMIT];
        let retired_ring = {
            let mut registry = self.namespace.packets().state.lock();
            let state = registry
                .endpoints
                .get_mut(&self.id)
                .ok_or(SocketError::NotConnected)?;
            if ring.is_some() && state.ring.is_some() {
                return Err(SocketError::Busy);
            }
            if ring.is_some() {
                for (retired, packet) in retired_packets.iter_mut().zip(state.queue.drain(..)) {
                    *retired = Some(packet);
                }
            }
            core::mem::replace(&mut state.ring, ring)
        };
        drop(retired_ring);
        drop(retired_packets);
        Ok(())
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
        let owner = self.namespace.packets();
        let retired = {
            let mut registry = owner.state.lock();
            let retired = registry.endpoints.remove(&self.id);
            if retired.is_some() && registry.endpoints.is_empty() {
                owner.active.store(false, Ordering::Release);
            }
            retired
        };
        drop(retired);
    }
}

/// 在网络层解析前将一个 Ethernet frame 镜像给匹配的 packet endpoints。
///
/// # Parameters
///
/// - `frame`: 包含 Ethernet header 的完整 RX frame。
///
/// # Errors
///
/// 不返回错误。仅匹配本域及相应接口和协议，抓包队列已满时只丢弃镜像。
pub(crate) fn deliver(
    owner: &PacketRegistry,
    index: u32,
    mac: [u8; 6],
    frame: &[u8],
    checksum_valid: bool,
) {
    if !owner.has_endpoints() {
        return;
    }
    let Ok(frame) = EthernetFrame::parse(frame) else {
        return;
    };
    let protocol = frame.type_or_length();
    let source = PacketAddress {
        protocol: protocol.to_be(),
        interface_index: index as i32,
        hardware_type: ARPHRD_ETHER,
        packet_type: packet_type(&frame.destination(), mac),
        address_length: 6,
        address: padded_address(frame.source()),
    };
    deliver_packet(
        owner,
        source,
        *frame.header(),
        frame.payload(),
        None,
        if checksum_valid {
            PacketChecksum::Verified
        } else {
            PacketChecksum::Unverified
        },
    );
}
/// 实际IP-medium lo tap沿原生sockaddr_ll发布，不伪造Ethernet/ARP协议设备。
pub(crate) fn deliver_loopback(
    owner: &PacketRegistry,
    header: [u8; ETH_HEADER_LENGTH],
    packet: &[u8],
) {
    if !owner.has_endpoints() {
        return;
    }
    let frame = EthernetFrame::parse(&header).expect("fixed packet link header");
    deliver_packet(
        owner,
        PacketAddress {
            protocol: frame.type_or_length().to_be(),
            interface_index: 1,
            hardware_type: 772,
            packet_type: packet_type(&frame.destination(), [0; 6]),
            address_length: 6,
            address: padded_address(frame.source()),
        },
        header,
        packet,
        None,
        PacketChecksum::Unverified,
    );
}
/// 镜像交给实际发送路径的 L2 报文，排除发起该发送的 socket。
/// 返回是否新登记了待交付的可读通知。
pub(crate) fn deliver_outgoing(
    owner: &PacketRegistry,
    index: u32,
    hardware_type: u16,
    header: [u8; ETH_HEADER_LENGTH],
    payload: &[u8],
    checksum_partial: bool,
    sender: Option<usize>,
) -> bool {
    if !owner.has_endpoints() {
        return false;
    }
    let frame = EthernetFrame::parse(&header).expect("fixed packet link header");
    let source = PacketAddress {
        protocol: frame.type_or_length().to_be(),
        interface_index: index as i32,
        hardware_type,
        packet_type: PACKET_OUTGOING,
        address_length: 6,
        address: padded_address(frame.source()),
    };
    deliver_packet(
        owner,
        source,
        header,
        payload,
        sender,
        if checksum_partial {
            PacketChecksum::Partial
        } else {
            PacketChecksum::Unverified
        },
    )
}

fn matches(state: &EndpointState, source: PacketAddress) -> bool {
    (state.interface_index == 0 || state.interface_index == source.interface_index)
        && state.protocol != 0
        && (u16::from_be(state.protocol) == ETH_P_ALL || state.protocol == source.protocol)
}
fn deliver_packet(
    owner: &PacketRegistry,
    source: PacketAddress,
    header: [u8; ETH_HEADER_LENGTH],
    packet: &[u8],
    sender: Option<usize>,
    checksum: PacketChecksum,
) -> bool {
    let matches = |id, state: &EndpointState| sender != Some(id) && matches(state, source);
    let needs_queue = {
        let mut registry = owner.state.lock();
        let mut matched = false;
        let mut has_ring_room = false;
        let needs_queue = registry.endpoints.iter().any(|(&id, state)| {
            if !matches(id, state) {
                return false;
            }
            matched = true;
            if let Some(ring) = &state.ring {
                has_ring_room |= ring.can_receive();
                false
            } else {
                state.queue.len() < RECEIVE_QUEUE_LIMIT
            }
        });
        if !needs_queue && !has_ring_room {
            if matched {
                registry.endpoints.for_each_mut(|&id, state| {
                    if matches(id, state) {
                        state.observed = state.observed.wrapping_add(1);
                        state.dropped = state.dropped.wrapping_add(1);
                    }
                });
            }
            return false;
        }
        needs_queue
    };
    // UTC 与载荷在首次匹配后、registry guard 外取得。无消费者时不调用时钟。
    let timestamp_ns = native_runtime::system::utc().max(0) as u64;
    let payload = needs_queue
        .then(|| {
            let mut bytes = Vec::new();
            bytes.try_reserve_exact(packet.len()).ok()?;
            bytes.extend_from_slice(packet);
            Arc::try_new(SharedPacket {
                link_header: header,
                payload: bytes,
                source,
                checksum,
                timestamp_ns,
            })
            .ok()
        })
        .flatten();
    let mut edge = false;
    let mut registry = owner.state.lock();
    registry.endpoints.for_each_mut(|&id, state| {
        if !matches(id, state) {
            return;
        }
        state.observed = state.observed.wrapping_add(1);
        if let Some(ring) = &mut state.ring {
            if let Some(notify) = ring.push(
                source,
                &header,
                packet,
                timestamp_ns,
                checksum,
                state.dropped != 0,
            ) {
                edge |= notify && !state.notification_pending;
                state.notification_pending |= notify;
            } else {
                state.dropped = state.dropped.wrapping_add(1);
            }
            return;
        }
        let Some(payload) = payload
            .as_ref()
            .filter(|_| state.queue.len() < RECEIVE_QUEUE_LIMIT)
        else {
            state.dropped = state.dropped.wrapping_add(1);
            return;
        };
        let was_empty = state.queue.is_empty();
        assert!(
            state.queue.capacity() >= RECEIVE_QUEUE_LIMIT,
            "packet queue admission lost capacity"
        );
        state.queue.push_back(payload.clone());
        edge |= was_empty && !state.notification_pending;
        state.notification_pending |= was_empty;
    });
    edge
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
pub(crate) fn publish_transmit_ready(owner: &PacketRegistry, index: u32) {
    if !owner.has_endpoints() {
        return;
    }
    owner.state.lock().endpoints.for_each_mut(|_, state| {
        if state.interface_index == 0 || state.interface_index == index as i32 {
            state.notification_pending = true;
        }
    });
}

/// NETDEV_UNREGISTER：绑定该接口的 endpoint 解除绑定（ifindex -1）并报告 ENETDOWN，与 Linux
/// `packet_notifier` 一致；edge 由随后的统一 drain 在 registry lock 外发布。
pub(crate) fn publish_interface_removed(owner: &PacketRegistry, index: u32) {
    if !owner.has_endpoints() {
        return;
    }
    owner.state.lock().endpoints.for_each_mut(|_, state| {
        if state.interface_index == index as i32 {
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
pub(crate) fn take_pending_notification(
    owner: &PacketRegistry,
    after: usize,
) -> Option<(usize, Arc<PacketSocket>)> {
    if !owner.has_endpoints() {
        return None;
    }
    let mut registry = owner.state.lock();
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

pub(crate) fn network_error(error: NetworkError) -> SocketError {
    match error {
        NetworkError::InvalidConfiguration => SocketError::Invalid,
        NetworkError::WouldBlock => SocketError::Again,
        NetworkError::NoMemory => SocketError::NoMemory,
        NetworkError::NoResources => SocketError::NoBuffer,
        NetworkError::FrameTooLarge => SocketError::MessageTooLarge,
        NetworkError::Device => SocketError::Device,
        NetworkError::Disconnected => SocketError::NetworkDown,
    }
}
