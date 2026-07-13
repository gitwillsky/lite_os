use alloc::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Weak},
    vec,
    vec::Vec,
};
use spin::{Mutex, Once};

use crate::{
    drivers::network::{NetworkDevice, NetworkError, network_device},
    ipc::{PipeDirection, PipeEnd},
};

use super::{PacketAddress, SocketError, SocketPollState, SocketWaitSource};

const ETH_HEADER_LENGTH: usize = 14;
const ETH_PAYLOAD_MTU: usize = 1500;
const ETH_P_IP: u16 = 0x0800;
const ARPHRD_ETHER: u16 = 1;
const INTERFACE_INDEX: i32 = 1;
const PACKET_HOST: u8 = 0;
const PACKET_BROADCAST: u8 = 1;
const PACKET_MULTICAST: u8 = 2;
const PACKET_OTHERHOST: u8 = 3;
const RECEIVE_QUEUE_LIMIT: usize = 64;

struct Packet {
    payload: Vec<u8>,
    source: PacketAddress,
}

struct EndpointState {
    endpoint: Weak<PacketSocket>,
    protocol: u16,
    interface_index: i32,
    queue: VecDeque<Packet>,
}

struct PacketRegistry {
    device: Arc<dyn NetworkDevice>,
    endpoints: BTreeMap<usize, EndpointState>,
    next_id: usize,
}

// OWNER: PacketRegistry uniquely owns AF_PACKET endpoint bindings and receive queues. Ethernet
// frames are mirrored here once before smoltcp consumes them; duplicating these queues in the IPv4
// stack would make packet-socket delivery depend on L3 acceptance and lose Linux layer semantics.
static PACKET_REGISTRY: Once<Mutex<PacketRegistry>> = Once::new();

fn registry() -> Result<&'static Mutex<PacketRegistry>, SocketError> {
    PACKET_REGISTRY.get().ok_or(SocketError::NetworkUnreachable)
}

/// @description 初始化唯一 AF_PACKET registry，并共享 DTB 选中的 Ethernet device seam。
/// @return device 已发现时发布 registry；无网络设备时保持未初始化。
/// @errors 无返回错误；重复调用由 Once 保持幂等。
pub(super) fn init() {
    let Some(device) = network_device() else {
        return;
    };
    PACKET_REGISTRY.call_once(|| {
        Mutex::new(PacketRegistry {
            device,
            endpoints: BTreeMap::new(),
            next_id: 1,
        })
    });
}

/// @description Linux AF_PACKET/SOCK_DGRAM endpoint；只暴露去除 Ethernet header 的 L3 packet。
pub(super) struct PacketSocket {
    id: usize,
    notify_read: Arc<PipeEnd>,
    notify_write: Arc<PipeEnd>,
}

impl PacketSocket {
    /// @description 创建绑定指定 network-byte-order protocol 的 packet endpoint。
    /// @param protocol `socket(2)` 传入的 network-byte-order EtherType。
    /// @param notify endpoint 独占的 readiness notification Pipe。
    /// @return 已注册且可被 RX tap 发现的 endpoint Arc。
    /// @errors protocol 不是 IPv4、registry 未初始化或 id 耗尽时返回错误。
    pub(super) fn new(
        protocol: usize,
        notify: (Arc<PipeEnd>, Arc<PipeEnd>),
    ) -> Result<Arc<Self>, SocketError> {
        let protocol = u16::try_from(protocol).map_err(|_| SocketError::ProtocolNotSupported)?;
        if u16::from_be(protocol) != ETH_P_IP {
            return Err(SocketError::ProtocolNotSupported);
        }
        let mut registry = registry()?.lock();
        let id = registry.next_id;
        registry.next_id = registry
            .next_id
            .checked_add(1)
            .ok_or(SocketError::NoMemory)?;
        let endpoint = Arc::new(Self {
            id,
            notify_read: notify.0,
            notify_write: notify.1,
        });
        registry.endpoints.insert(
            id,
            EndpointState {
                endpoint: Arc::downgrade(&endpoint),
                protocol,
                interface_index: 0,
                queue: VecDeque::new(),
            },
        );
        Ok(endpoint)
    }

    /// @description 将 endpoint 绑定到唯一 Ethernet interface 与 IPv4 EtherType。
    /// @param address userspace `sockaddr_ll` 的完整语义值。
    /// @return 首次有效绑定返回 unit。
    /// @errors interface、protocol、hardware address 形状或重复绑定无效时返回错误。
    pub(super) fn bind(&self, address: PacketAddress) -> Result<(), SocketError> {
        if address.interface_index != INTERFACE_INDEX
            || u16::from_be(address.protocol) != ETH_P_IP
            || address.address_length > 8
        {
            return Err(SocketError::AddressNotAvailable);
        }
        let mut registry = registry()?.lock();
        let state = registry
            .endpoints
            .get_mut(&self.id)
            .ok_or(SocketError::NotConnected)?;
        if state.interface_index != 0 {
            return Err(SocketError::Invalid);
        }
        if state.protocol != address.protocol {
            return Err(SocketError::ProtocolNotSupported);
        }
        state.interface_index = address.interface_index;
        Ok(())
    }

    /// @description 返回 endpoint 权威 `sockaddr_ll` binding。
    /// @return 未 bind 时 ifindex 为零，其余字段保持 Linux packet socket 形状。
    /// @errors registry 或 endpoint 已消失时返回错误。
    pub(super) fn address(&self) -> Result<PacketAddress, SocketError> {
        let registry = registry()?.lock();
        let state = registry
            .endpoints
            .get(&self.id)
            .ok_or(SocketError::NotConnected)?;
        Ok(PacketAddress {
            protocol: state.protocol,
            interface_index: state.interface_index,
            hardware_type: ARPHRD_ETHER,
            packet_type: PACKET_HOST,
            address_length: 6,
            address: padded_address(registry.device.mac_address()),
        })
    }

    /// @description 以 SOCK_DGRAM 语义发送一个无 Ethernet header 的 L3 packet。
    /// @param input 完整 IPv4 packet。
    /// @param target 必须包含唯一 interface 与六字节 destination MAC。
    /// @return 成功提交的 L3 byte count。
    /// @errors target/MTU 无效或 adapter 发送失败时返回标准 socket error。
    pub(super) fn send_to(
        &self,
        input: &[u8],
        target: Option<PacketAddress>,
    ) -> Result<usize, SocketError> {
        if input.len() > ETH_PAYLOAD_MTU {
            return Err(SocketError::MessageTooLarge);
        }
        let target = target.ok_or(SocketError::DestinationRequired)?;
        if target.interface_index != INTERFACE_INDEX
            || target.address_length != 6
            || u16::from_be(target.protocol) != ETH_P_IP
        {
            return Err(SocketError::AddressNotAvailable);
        }
        let registry = registry()?.lock();
        let state = registry
            .endpoints
            .get(&self.id)
            .ok_or(SocketError::NotConnected)?;
        if state.interface_index != 0 && state.interface_index != target.interface_index {
            return Err(SocketError::AddressNotAvailable);
        }
        if state.protocol != target.protocol {
            return Err(SocketError::ProtocolNotSupported);
        }
        let mut frame = vec![0u8; ETH_HEADER_LENGTH + input.len()];
        frame[..6].copy_from_slice(&target.address[..6]);
        frame[6..12].copy_from_slice(&registry.device.mac_address());
        frame[12..14].copy_from_slice(&ETH_P_IP.to_be_bytes());
        frame[ETH_HEADER_LENGTH..].copy_from_slice(input);
        registry.device.transmit(&frame).map_err(network_error)?;
        Ok(input.len())
    }

    /// @description 接收或窥视一个无 Ethernet header 的 packet，并保留原始 packet 长度。
    /// @param output kernel-owned 输出 buffer。
    /// @param peek true 时不消费 queue head。
    /// @return copied/full length 与 source `sockaddr_ll`。
    /// @errors queue 为空返回 Again；registry 状态损坏返回 NotConnected。
    pub(super) fn receive(
        &self,
        output: &mut [u8],
        peek: bool,
    ) -> Result<(usize, usize, PacketAddress), SocketError> {
        let mut registry = registry()?.lock();
        let state = registry
            .endpoints
            .get_mut(&self.id)
            .ok_or(SocketError::NotConnected)?;
        let packet = state.queue.front().ok_or(SocketError::Again)?;
        let full_length = packet.payload.len();
        let count = output.len().min(full_length);
        output[..count].copy_from_slice(&packet.payload[..count]);
        let source = packet.source;
        if !peek {
            state.queue.pop_front();
        }
        let drained = !peek && state.queue.is_empty();
        drop(registry);
        if drained {
            self.consume_notify();
        }
        Ok((count, full_length, source))
    }

    /// @description 从唯一 receive queue 投影 packet endpoint readiness。
    /// @return queue 非空时 readable，adapter 存在时始终 writable。
    /// @errors registry 消失投影为 not-readable；发送时再返回具体错误。
    pub(super) fn poll_state(&self) -> SocketPollState {
        let readable = registry().is_ok_and(|registry| {
            registry
                .lock()
                .endpoints
                .get(&self.id)
                .is_some_and(|state| !state.queue.is_empty())
        });
        SocketPollState {
            readable,
            writable: true,
            hangup: false,
            error: false,
        }
    }

    /// @description 读取 packet endpoint notification source 的 readiness generation。
    /// @return Pipe read side 当前 generation。
    /// @errors 无错误。
    pub(super) fn readiness_generation(&self) -> u64 {
        self.notify_read
            .pipe()
            .readiness_generation(PipeDirection::Read)
    }

    /// @description 把 packet notification Pipe 投影给统一 OFD wait seam。
    /// @return 单一 read-direction wait source。
    /// @errors 无错误。
    pub(super) fn wait_sources(&self) -> Vec<SocketWaitSource> {
        vec![SocketWaitSource::Notification(self.notify_read.pipe())]
    }

    /// @description 在 registry lock 已释放后发布一次 level-triggered readable 通知。
    /// @return 无返回值。
    /// @errors notification 已存在或 Pipe 已关闭时幂等忽略。
    pub(super) fn notify(&self) {
        self.notify_write.signal_readiness();
    }

    fn consume_notify(&self) {
        self.consume_wait_notifications();
    }

    /// @description 排空已观察的 packet readiness edge，供统一 poll registration 在 owner lock 内执行。
    ///
    /// @return 无返回值；实际 queue readiness 由随后的 level recheck 决定。
    pub(super) fn consume_wait_notifications(&self) {
        self.notify_read.drain_readiness();
    }
}

impl Drop for PacketSocket {
    fn drop(&mut self) {
        if let Some(registry) = PACKET_REGISTRY.get() {
            registry.lock().endpoints.remove(&self.id);
        }
    }
}

/// @description 在 smoltcp 解析前将一个 Ethernet frame 镜像给匹配的 packet endpoints。
/// @param frame 包含 Ethernet header 的完整 RX frame。
/// @return 本轮从 empty 转为 readable、且需在 NetworkStack 解锁后唤醒的 endpoints。
/// @errors 损坏、非 IPv4、未绑定或队列已满的 frame 被丢弃，不改变 L3 ingress。
pub(super) fn deliver(frame: &[u8]) -> Vec<Arc<PacketSocket>> {
    if frame.len() < ETH_HEADER_LENGTH || u16::from_be_bytes([frame[12], frame[13]]) != ETH_P_IP {
        return Vec::new();
    }
    let Some(registry) = PACKET_REGISTRY.get() else {
        return Vec::new();
    };
    let mut registry = registry.lock();
    let own_mac = registry.device.mac_address();
    let packet_type = packet_type(&frame[..6], own_mac);
    let source = PacketAddress {
        protocol: u16::to_be(ETH_P_IP),
        interface_index: INTERFACE_INDEX,
        hardware_type: ARPHRD_ETHER,
        packet_type,
        address_length: 6,
        address: padded_address(frame[6..12].try_into().unwrap()),
    };
    let mut notifications = Vec::new();
    for state in registry.endpoints.values_mut() {
        if state.interface_index != INTERFACE_INDEX
            || u16::from_be(state.protocol) != ETH_P_IP
            || state.queue.len() >= RECEIVE_QUEUE_LIMIT
        {
            continue;
        }
        let was_empty = state.queue.is_empty();
        state.queue.push_back(Packet {
            payload: frame[ETH_HEADER_LENGTH..].to_vec(),
            source,
        });
        if was_empty && let Some(endpoint) = state.endpoint.upgrade() {
            notifications.push(endpoint);
        }
    }
    notifications
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
        NetworkError::Device => SocketError::NetworkUnreachable,
    }
}
