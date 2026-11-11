//! 实际IP-medium loopback队列；没有伪Ethernet/ARP或第二份transport owner。
use super::ingress::IngressFacts;
use alloc::vec::Vec;
use core::cell::{Cell, RefCell};
use device_model::network::NetworkStatistics;
use network_protocol::SocketError;
use smoltcp::{
    phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken},
    storage::{PacketBuffer, PacketMetadata},
    time::Instant,
};

pub(super) const LOOPBACK_MTU: usize = 65_536;
const LOOPBACK_QUEUE_BYTES: usize = 4 * LOOPBACK_MTU;
const LOOPBACK_QUEUE_PACKETS: usize = 64;

/// 唯一queue metadata冻结物理link header及LOCAL原logical iif；AF_PACKET只消费header。
#[derive(Clone, Copy)]
struct Delivery {
    header: [u8; 14],
    interface_index: u32,
}
impl Delivery {
    fn accepts_ipv6(self) -> bool {
        self.header[12..14] == [0x86, 0xdd]
            && (self.header[..6] == [0; 6] || self.header[0] & 1 != 0)
    }
    fn accepts_ipv4(self) -> bool {
        self.header[12..14] == [8, 0] && (self.header[..6] == [0; 6] || self.header[0] & 1 != 0)
    }
}

/// 唯一NET protocol mutex串行化的loopback设备；token只借用当前一次callback。
pub(crate) struct LoopbackDevice {
    net_id: u64,
    // OWNER: actual lo MTU，由ProtocolDevice在唯一NET guard内配置；storage容量仍为最大frame。
    pub(super) mtu: usize,
    queue: RefCell<PacketBuffer<'static, Delivery>>,
    // OWNER: 原生ingress reply不保证有TX reservation；拥塞时用预留scratch完成codec并丢帧，
    // 不能让库的不可失败TxToken触发OOM/panic，也不能覆盖尚未接收的IP packet。
    scratch: RefCell<Vec<u8>>,
    // OWNER: ingress payload必须在释放queue borrow后进入库callback，callback可能立即发送回复。
    // RX与TX scratch不能共用，否则回复会覆写尚在解析的输入IP packet。
    receive_scratch: RefCell<Vec<u8>>,
    // OWNER: 唯一 RX scratch 的活跃借用身份；NET guard 串行化领取，token Drop 才归还。
    // 批 API 可保留已发出的 token；缺少该状态会为同一 queue front 发出多个 token，造成重复出队/覆盖 scratch。
    receive_claimed: Cell<bool>,
    queued_packets: Cell<usize>,
    dropped_packets: Cell<u64>,
    statistics: Cell<NetworkStatistics>,
}

impl LoopbackDevice {
    /// 在原 NET guard 下借用回环 TX；返回独占写入 token，队列满则 None，不复制槽或增加锁。
    pub(super) fn transmit_shared(&self, _: Instant) -> Option<LoopbackTxToken<'_>> {
        self.can_transmit().then_some(LoopbackTxToken {
            device: self,
            interface_index: 1,
        })
    }
    pub(super) fn can_receive(&self) -> bool {
        self.queued_packets.get() != 0 && !self.receive_claimed.get()
    }
    pub(super) fn receive_one(&self) -> Option<(LoopbackRxToken<'_>, LoopbackTxToken<'_>)> {
        if !self.can_receive() {
            return None;
        }
        let facts = {
            let mut queue = self.queue.borrow_mut();
            let (delivery, packet) = queue.peek().ok()?;
            // 与消费时相同的链路准入；未知 packet 协议仍保留原始抓包，不产生 IP 路由事实。
            if delivery.accepts_ipv4() {
                packet_wire::ipv4::Ipv4Packet::parse_header(packet)
                    .ok()
                    .map(IngressFacts::from_ipv4)
            } else {
                None
            }
        };
        self.receive_claimed.set(true);
        Some((
            LoopbackRxToken {
                device: self,
                facts,
            },
            LoopbackTxToken {
                device: self,
                interface_index: 1,
            },
        ))
    }
    /// publication前准备固定buffer；失败不注册设备或进入任何SocketSet。
    pub(super) fn prepare(net_id: u64) -> Result<Self, SocketError> {
        let mut metadata = Vec::new();
        metadata
            .try_reserve_exact(2 * LOOPBACK_QUEUE_PACKETS)
            .map_err(|_| SocketError::NoMemory)?;
        metadata.resize(2 * LOOPBACK_QUEUE_PACKETS, PacketMetadata::EMPTY);
        let mut payload = Vec::new();
        payload
            .try_reserve_exact(LOOPBACK_QUEUE_BYTES)
            .map_err(|_| SocketError::NoMemory)?;
        payload.resize(LOOPBACK_QUEUE_BYTES, 0);
        let mut scratch = Vec::new();
        scratch
            .try_reserve_exact(LOOPBACK_MTU)
            .map_err(|_| SocketError::NoMemory)?;
        scratch.resize(LOOPBACK_MTU, 0);
        let mut receive_scratch = Vec::new();
        receive_scratch
            .try_reserve_exact(LOOPBACK_MTU)
            .map_err(|_| SocketError::NoMemory)?;
        receive_scratch.resize(LOOPBACK_MTU, 0);
        Ok(Self {
            net_id,
            mtu: LOOPBACK_MTU,
            queue: RefCell::new(PacketBuffer::new(metadata, payload)),
            scratch: RefCell::new(scratch),
            receive_scratch: RefCell::new(receive_scratch),
            receive_claimed: Cell::new(false),
            queued_packets: Cell::new(0),
            dropped_packets: Cell::new(0),
            statistics: Cell::new(NetworkStatistics::default()),
        })
    }
    pub(super) fn record_transmit_drop(&self) {
        self.dropped_packets
            .set(self.dropped_packets.get().saturating_add(1));
    }
    pub(super) fn statistics(&self) -> NetworkStatistics {
        let mut statistics = self.statistics.get();
        statistics.transmitted_dropped = self.dropped_packets.get();
        statistics
    }
    pub(crate) fn can_transmit(&self) -> bool {
        self.queued_packets.get() < LOOPBACK_QUEUE_PACKETS
            && self.queue.borrow().payload_bytes_count() <= LOOPBACK_QUEUE_BYTES - 2 * LOOPBACK_MTU
    }
    /// packet socket与IP transport共用同一实际queue准入，不建立第二份loopback设备。
    pub(crate) fn inject(
        &self,
        header: [u8; 14],
        packet: &[u8],
        sender: usize,
    ) -> Result<(), SocketError> {
        if packet.len() > self.mtu {
            return Err(SocketError::MessageTooLarge);
        }
        if !self.can_transmit() {
            return Err(SocketError::Again);
        }
        let (_, sent) = self.with_transmit(header, 1, Some(sender), packet.len(), |output| {
            output.copy_from_slice(packet)
        });
        if sent {
            Ok(())
        } else {
            Err(SocketError::Again)
        }
    }
    fn with_transmit<R>(
        &self,
        header: [u8; 14],
        interface_index: u32,
        sender: Option<usize>,
        length: usize,
        operation: impl FnOnce(&mut [u8]) -> R,
    ) -> (R, bool) {
        assert!(length <= self.mtu, "IP packet exceeds actual loopback MTU");
        let encode = |packet: &mut [u8]| {
            let result = operation(packet);
            crate::packet::deliver_outgoing(self.net_id, 1, 772, header, packet, false, sender);
            result
        };
        let mut queue = self.queue.borrow_mut();
        let admitted = self.queued_packets.get() < LOOPBACK_QUEUE_PACKETS;
        let result = match admitted
            .then(|| {
                queue.enqueue(
                    length,
                    Delivery {
                        header,
                        interface_index,
                    },
                )
            })
            .transpose()
        {
            Ok(Some(packet)) => {
                self.queued_packets.set(self.queued_packets.get() + 1);
                (encode(packet), true)
            }
            _ => (encode(&mut self.scratch.borrow_mut()[..length]), false),
        };
        let mut statistics = self.statistics.get();
        if result.1 {
            statistics.transmitted_bytes =
                statistics.transmitted_bytes.saturating_add(length as u64);
            statistics.transmitted_packets = statistics.transmitted_packets.saturating_add(1);
        }
        self.statistics.set(statistics);
        crate::worker::request_poll();
        result
    }
}

pub(crate) struct LoopbackRxToken<'a> {
    device: &'a LoopbackDevice,
    // OWNER: claim 保证队列头在本 token 内不出队；格式事实只随本次借用存续，不跨领取缓存。
    pub(super) facts: Option<IngressFacts>,
}
impl LoopbackRxToken<'_> {
    /// 只借当前队首作本机分片识别；claim 保证视图在原 RX token 存续期不出队。
    pub(super) fn with_frame<R>(&self, inspect: impl FnOnce(&[u8]) -> R) -> R {
        let mut queue = self.device.queue.borrow_mut();
        let (delivery, packet) = queue.peek().expect("claimed loopback packet exists");
        inspect(if delivery.accepts_ipv4() || delivery.accepts_ipv6() {
            packet
        } else {
            &[]
        })
    }
    pub(super) fn ipv6(&self) -> bool {
        self.device
            .queue
            .borrow_mut()
            .peek()
            .is_ok_and(|(delivery, _)| delivery.accepts_ipv6())
    }
}
impl Drop for LoopbackRxToken<'_> {
    fn drop(&mut self) {
        // consume 的 scratch/queue 局部借用已先退出；未 consume 的 token 则不出队，只撤销领取。
        self.device.receive_claimed.set(false);
    }
}
pub(crate) struct LoopbackTxToken<'a> {
    device: &'a LoopbackDevice,
    interface_index: u32,
}

impl RxToken for LoopbackRxToken<'_> {
    fn meta(&self) -> smoltcp::phy::PacketMeta {
        let mut meta = smoltcp::phy::PacketMeta::default();
        meta.interface_index = self
            .device
            .queue
            .borrow_mut()
            .peek()
            .ok()
            .map(|(delivery, _)| delivery.interface_index);
        meta
    }
    fn consume<R, F>(self, operation: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        let mut input = self.device.receive_scratch.borrow_mut();
        let (length, header) = {
            let mut queue = self.device.queue.borrow_mut();
            let (header, packet) = queue
                .dequeue()
                .expect("唯一loopback RxToken必须消费已有packet");
            let length = packet.len();
            input[..length].copy_from_slice(packet);
            (length, header)
        };
        let packet = &input[..length];
        self.device.queued_packets.set(
            self.device
                .queued_packets
                .get()
                .checked_sub(1)
                .expect("loopback queue double receive"),
        );
        let mut statistics = self.device.statistics.get();
        statistics.received_bytes = statistics
            .received_bytes
            .saturating_add(packet.len() as u64);
        statistics.received_packets = statistics.received_packets.saturating_add(1);
        self.device.statistics.set(statistics);
        crate::packet::deliver_loopback(self.device.net_id, header.header, packet);
        // IP-medium 的 IPv4/IPv6 由各自入口消费；其他协议仅由 tap 接收。
        operation(if header.accepts_ipv4() || header.accepts_ipv6() {
            packet
        } else {
            &[]
        })
    }
}
impl TxToken for LoopbackTxToken<'_> {
    fn set_meta(&mut self, meta: smoltcp::phy::PacketMeta) {
        self.interface_index = meta.interface_index.unwrap_or(1);
    }
    fn consume<R, F>(self, length: usize, operation: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let device = self.device;
        let (result, sent) = self.consume_ip(length, 0x0800, operation);
        if !sent {
            device.record_transmit_drop();
        }
        result
    }
}
impl LoopbackTxToken<'_> {
    pub(super) fn consume_ip<R>(
        self,
        length: usize,
        protocol: u16,
        operation: impl FnOnce(&mut [u8]) -> R,
    ) -> (R, bool) {
        let mut header = [0; 14];
        header[12..14].copy_from_slice(&protocol.to_be_bytes());
        self.device
            .with_transmit(header, self.interface_index, None, length, operation)
    }
}
impl Device for LoopbackDevice {
    type RxToken<'a> = LoopbackRxToken<'a>;
    type TxToken<'a> = LoopbackTxToken<'a>;
    fn receive(&mut self, _: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        self.receive_one()
    }
    fn transmit(&mut self, timestamp: Instant) -> Option<Self::TxToken<'_>> {
        self.transmit_shared(timestamp)
    }
    fn capabilities(&self) -> DeviceCapabilities {
        let mut capabilities = DeviceCapabilities::default();
        capabilities.medium = Medium::Ip;
        capabilities.max_transmission_unit = self.mtu;
        capabilities
    }
}
