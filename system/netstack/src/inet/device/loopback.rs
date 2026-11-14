//! 实际IP-medium loopback队列；没有伪Ethernet/ARP或第二份transport owner。
use crate::inet::datagram::Queue;
use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::cell::{Cell, RefCell};
use device_model::network::{NetworkError, NetworkStatistics};
use network_protocol::SocketError;
use packet_wire::batch::IngressIp;

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
    packets: Arc<crate::packet::PacketRegistry>,
    // OWNER: actual lo MTU，由ProtocolDevice在唯一NET guard内配置；storage容量仍为最大frame。
    pub(super) mtu: usize,
    // 回环独占队列记录，物理接口无需为这些记录保留内联空间。
    queue: RefCell<Box<Queue<Delivery, LOOPBACK_QUEUE_PACKETS>>>,
    // OWNER: 入站回调可能发回环回复，独立 RX 缓冲允许回调期间归还队列借用。
    receive_scratch: RefCell<Vec<u8>>,
    // OWNER: 唯一 RX scratch 的活跃借用身份；NET guard 串行化领取，token Drop 才归还。
    // 批 API 可保留已发出的 token；缺少该状态会为同一 queue front 发出多个 token，造成重复出队/覆盖 scratch。
    receive_claimed: Cell<bool>,
    dropped_packets: Cell<u64>,
    statistics: Cell<NetworkStatistics>,
}

impl LoopbackDevice {
    /// 检查当前回环容量。具体发送窗口在提交时按报文长度取得。
    pub(super) fn reserve(&self) -> Option<LoopbackTxToken<'_>> {
        self.can_transmit().then_some(LoopbackTxToken {
            device: self,
            interface_index: 1,
        })
    }
    pub(super) fn can_receive(&self) -> bool {
        !self.receive_claimed.get() && !self.queue.borrow().is_empty()
    }
    pub(super) fn receive_one(&self) -> Option<(LoopbackRxToken<'_>, LoopbackTxToken<'_>)> {
        if !self.can_receive() {
            return None;
        }
        let facts = {
            let queue = self.queue.borrow();
            let (delivery, packet) = queue.peek()?;
            // 与消费时相同的链路准入；未知 packet 协议仍保留原始抓包，不产生 IP 路由事实。
            if delivery.accepts_ipv4() {
                packet_wire::ipv4::Ipv4Packet::parse_header(packet)
                    .ok()
                    .map(IngressIp::from_ipv4)
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
    /// 发布前准备固定缓冲区；失败时设备尚未注册。
    pub(super) fn prepare(
        net_id: u64,
        packets: Arc<crate::packet::PacketRegistry>,
    ) -> Result<Self, SocketError> {
        let mut queue = Queue::new();
        queue.prepare(LOOPBACK_QUEUE_BYTES)?;
        let mut receive_scratch = Vec::new();
        receive_scratch
            .try_reserve_exact(LOOPBACK_MTU)
            .map_err(|_| SocketError::NoMemory)?;
        receive_scratch.resize(LOOPBACK_MTU, 0);
        Ok(Self {
            net_id,
            packets,
            mtu: LOOPBACK_MTU,
            queue: RefCell::new(Box::try_new(queue).map_err(|_| SocketError::NoMemory)?),
            receive_scratch: RefCell::new(receive_scratch),
            receive_claimed: Cell::new(false),
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
        let queue = self.queue.borrow();
        queue.can_send() && queue.used_bytes() <= LOOPBACK_QUEUE_BYTES - 2 * LOOPBACK_MTU
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
        self.with_transmit(header, 1, Some(sender), packet.len(), |output| {
            output.copy_from_slice(packet);
            true
        })
    }

    fn with_transmit(
        &self,
        header: [u8; 14],
        interface_index: u32,
        sender: Option<usize>,
        length: usize,
        operation: impl FnOnce(&mut [u8]) -> bool,
    ) -> Result<(), SocketError> {
        if length > self.mtu {
            return Err(SocketError::MessageTooLarge);
        }
        self.queue.borrow_mut().push_with(
            Delivery {
                header,
                interface_index,
            },
            length,
            |packet| {
                if !operation(packet) {
                    return Err(SocketError::Invalid);
                }
                crate::packet::deliver_outgoing(
                    &self.packets,
                    1,
                    772,
                    header,
                    packet,
                    false,
                    sender,
                );
                Ok(())
            },
        )?;
        let mut statistics = self.statistics.get();
        statistics.transmitted_bytes = statistics.transmitted_bytes.saturating_add(length as u64);
        statistics.transmitted_packets = statistics.transmitted_packets.saturating_add(1);
        self.statistics.set(statistics);
        crate::worker::request_poll(self.net_id);
        Ok(())
    }
}

pub(crate) struct LoopbackRxToken<'a> {
    device: &'a LoopbackDevice,
    // OWNER: claim 保证队列头在本 token 内不出队；格式事实只随本次借用存续，不跨领取缓存。
    pub(super) facts: Option<IngressIp>,
}
impl LoopbackRxToken<'_> {
    /// 只借当前队首作本机分片识别；claim 保证视图在原 RX token 存续期不出队。
    pub(super) fn with_frame<R>(&self, inspect: impl FnOnce(&[u8]) -> R) -> R {
        let queue = self.device.queue.borrow();
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
            .borrow()
            .peek()
            .is_some_and(|(delivery, _)| delivery.accepts_ipv6())
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

impl LoopbackRxToken<'_> {
    pub(super) fn interface_index(&self) -> Option<u32> {
        self.device
            .queue
            .borrow()
            .peek()
            .map(|(delivery, _)| delivery.interface_index)
    }

    pub(super) fn consume<R>(self, operation: impl FnOnce(&[u8]) -> R) -> R {
        let mut input = self.device.receive_scratch.borrow_mut();
        let (length, header) = {
            let mut queue = self.device.queue.borrow_mut();
            let (header, packet) = queue
                .peek()
                .expect("唯一loopback RxToken必须消费已有packet");
            let length = packet.len();
            input[..length].copy_from_slice(packet);
            queue.pop();
            (length, header)
        };
        let packet = &input[..length];
        let mut statistics = self.device.statistics.get();
        statistics.received_bytes = statistics
            .received_bytes
            .saturating_add(packet.len() as u64);
        statistics.received_packets = statistics.received_packets.saturating_add(1);
        self.device.statistics.set(statistics);
        crate::packet::deliver_loopback(&self.device.packets, header.header, packet);
        // IP-medium 的 IPv4/IPv6 由各自入口消费；其他协议仅由 tap 接收。
        operation(if header.accepts_ipv4() || header.accepts_ipv6() {
            packet
        } else {
            &[]
        })
    }
}
impl LoopbackTxToken<'_> {
    pub(super) fn set_interface(&mut self, interface: u32) {
        self.interface_index = interface;
    }

    pub(super) fn consume_ip(
        self,
        length: usize,
        protocol: u16,
        operation: impl FnOnce(&mut [u8]) -> bool,
    ) -> Result<(), NetworkError> {
        let mut header = [0; 14];
        header[12..14].copy_from_slice(&protocol.to_be_bytes());
        self.device
            .with_transmit(header, self.interface_index, None, length, operation)
            .map_err(|error| match error {
                SocketError::Again => NetworkError::WouldBlock,
                SocketError::MessageTooLarge => NetworkError::FrameTooLarge,
                _ => NetworkError::InvalidConfiguration,
            })
    }
}
