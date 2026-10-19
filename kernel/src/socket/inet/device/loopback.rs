//! 实际IP-medium loopback队列；没有伪Ethernet/ARP或第二份transport owner。
use super::TcpReset;
use crate::{drivers::network::NetworkStatistics, socket::SocketError};
use alloc::vec::Vec;
use core::cell::{Cell, RefCell};
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

/// 唯一NET protocol mutex串行化的loopback设备；token只借用当前一次callback。
pub(in crate::socket) struct LoopbackDevice {
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
    queued_packets: Cell<usize>,
    dropped_packets: Cell<u64>,
    statistics: Cell<NetworkStatistics>,
    ingress_reset: Cell<Option<TcpReset>>,
}

impl LoopbackDevice {
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
            queued_packets: Cell::new(0),
            dropped_packets: Cell::new(0),
            statistics: Cell::new(NetworkStatistics::default()),
            ingress_reset: Cell::new(None),
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
    pub(super) fn take_ingress_reset(&self) -> Option<TcpReset> {
        self.ingress_reset.replace(None)
    }
    pub(in crate::socket) fn can_transmit(&self) -> bool {
        self.queued_packets.get() < LOOPBACK_QUEUE_PACKETS
            && self.queue.borrow().payload_bytes_count() <= LOOPBACK_QUEUE_BYTES - 2 * LOOPBACK_MTU
    }
    /// packet socket与IP transport共用同一实际queue准入，不建立第二份loopback设备。
    pub(in crate::socket) fn inject(
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
        self.with_transmit(header, 1, Some(sender), packet.len(), |output| {
            output.copy_from_slice(packet)
        });
        Ok(())
    }
    fn with_transmit<R>(
        &self,
        header: [u8; 14],
        interface_index: u32,
        sender: Option<usize>,
        length: usize,
        operation: impl FnOnce(&mut [u8]) -> R,
    ) -> R {
        assert!(length <= self.mtu, "IP packet exceeds actual loopback MTU");
        let encode = |packet: &mut [u8]| {
            let result = operation(packet);
            crate::socket::packet::deliver_outgoing(
                self.net_id,
                1,
                772,
                header,
                packet,
                u16::from_be_bytes([header[12], header[13]]),
                sender,
            );
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
                encode(packet)
            }
            _ => {
                self.dropped_packets
                    .set(self.dropped_packets.get().saturating_add(1));
                encode(&mut self.scratch.borrow_mut()[..length])
            }
        };
        let mut statistics = self.statistics.get();
        statistics.transmitted_bytes = statistics.transmitted_bytes.saturating_add(length as u64);
        statistics.transmitted_packets = statistics.transmitted_packets.saturating_add(1);
        self.statistics.set(statistics);
        crate::drivers::network::request_poll();
        result
    }
}

pub(in crate::socket) struct LoopbackRxToken<'a>(&'a LoopbackDevice);
pub(in crate::socket) struct LoopbackTxToken<'a> {
    device: &'a LoopbackDevice,
    interface_index: u32,
}

impl RxToken for LoopbackRxToken<'_> {
    fn meta(&self) -> smoltcp::phy::PacketMeta {
        let mut meta = smoltcp::phy::PacketMeta::default();
        meta.interface_index = self
            .0
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
        let mut input = self.0.receive_scratch.borrow_mut();
        let (length, header) = {
            let mut queue = self.0.queue.borrow_mut();
            let (header, packet) = queue
                .dequeue()
                .expect("唯一loopback RxToken必须消费已有packet");
            let length = packet.len();
            input[..length].copy_from_slice(packet);
            (length, header.header)
        };
        let packet = &input[..length];
        self.0.queued_packets.set(
            self.0
                .queued_packets
                .get()
                .checked_sub(1)
                .expect("loopback queue double receive"),
        );
        self.0.ingress_reset.set(TcpReset::from_ip_packet(packet));
        let mut statistics = self.0.statistics.get();
        statistics.received_bytes = statistics
            .received_bytes
            .saturating_add(packet.len() as u64);
        statistics.received_packets = statistics.received_packets.saturating_add(1);
        self.0.statistics.set(statistics);
        crate::socket::packet::deliver_loopback(self.0.net_id, header, packet);
        // IP-medium仅解析ETH_P_IP；其他native packet协议仍由tap收到，不能误送进IPv4。
        operation(
            if header[12..14] == [8, 0] && (header[..6] == [0; 6] || header[0] & 1 != 0) {
                packet
            } else {
                &[]
            },
        )
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
        let mut header = [0; 14];
        header[12] = 8;
        self.device
            .with_transmit(header, self.interface_index, None, length, operation)
    }
}
impl Device for LoopbackDevice {
    type RxToken<'a> = LoopbackRxToken<'a>;
    type TxToken<'a> = LoopbackTxToken<'a>;
    fn receive(&mut self, _: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        self.ingress_reset.set(None);
        (self.queued_packets.get() != 0).then_some({
            (
                LoopbackRxToken(self),
                LoopbackTxToken {
                    device: self,
                    interface_index: 1,
                },
            )
        })
    }
    fn transmit(&mut self, _: Instant) -> Option<Self::TxToken<'_>> {
        self.can_transmit().then_some(LoopbackTxToken {
            device: self,
            interface_index: 1,
        })
    }
    fn capabilities(&self) -> DeviceCapabilities {
        let mut capabilities = DeviceCapabilities::default();
        capabilities.medium = Medium::Ip;
        capabilities.max_transmission_unit = self.mtu;
        capabilities
    }
}
