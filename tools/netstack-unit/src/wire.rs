use std::sync::{Mutex as HostMutex, OnceLock};
static SERIAL: HostMutex<()> = HostMutex::new(());
static NAMESPACE: OnceLock<Arc<crate::namespace::Namespace>> = OnceLock::new();
fn namespace() -> &'static Arc<crate::namespace::Namespace> { NAMESPACE.get().unwrap() }
fn initialize() {
    let scope = NAMESPACE.get_or_init(|| {
        crate::packet::init();
        let scope = crate::namespace::Namespace::create().unwrap();
        scope.install_protocol(prepare_network(&scope, &[Arc::new(Device)], true).unwrap());
        scope
    });
    TRANSMIT_AVAILABLE.store(true, core::sync::atomic::Ordering::Relaxed);
    { let mut wire = WIRE.lock().unwrap(); wire.0.clear(); wire.1.clear(); }
    configure_address(scope, 2, Ipv4Addr::new(10,0,2,15)).unwrap();
    configure_netmask(scope, 2, Ipv4Addr::new(255,255,255,0)).unwrap();
    configure_up(scope, 2, true).unwrap();
}
    type WireQueues = (std::collections::VecDeque<alloc::vec::Vec<u8>>, alloc::vec::Vec<alloc::vec::Vec<u8>>, FrameLoans);
    // 唯一边界 NIC 保存 wire frames；协议解析、握手和继承全部执行 production owners。
    pub(crate) static WIRE: std::sync::Mutex<WireQueues> = std::sync::Mutex::new((std::collections::VecDeque::new(), alloc::vec::Vec::new(), FrameLoans::new()));
    // NIC capacity 是边界输入；RST 重试和实际 handle 退休仍由 production owner 决定。
    pub(crate) static TRANSMIT_AVAILABLE: core::sync::atomic::AtomicBool =
        core::sync::atomic::AtomicBool::new(true);
    use alloc::sync::Arc;
    // 仅保活边界帧，token/Drop/提交走实际 device-model；无协议状态机的副本。
    pub(crate) struct FrameLoans {
        received: Option<alloc::vec::Vec<u8>>,
        transmit: alloc::vec::Vec<u8>,
        reserved: bool,
    }
    impl FrameLoans {
        const fn new() -> Self { Self { received: None, transmit: alloc::vec::Vec::new(), reserved: false } }
        fn receive<'a>(&mut self, owner: &'a dyn device_model::network::NetworkDevice, queue: u16, frame: alloc::vec::Vec<u8>) -> device_model::network::NetworkReceiveFrame<'a> {
            assert!(self.received.is_none());
            self.received = Some(frame);
            let frame = self.received.as_ref().unwrap();
            // SAFETY: Vec 的独立存储由 owner（或全局串行 fixture）保活，不 resize；只有 token 释放后才 take。
            unsafe { device_model::network::NetworkReceiveFrame::new(owner, queue, 0, 1, frame.as_ptr() as usize, frame.len()) }
        }
        fn reserve<'a>(&mut self, owner: &'a dyn device_model::network::NetworkDevice, queue: u16) -> Result<device_model::network::NetworkTransmit<'a>, device_model::network::NetworkError> {
            if self.reserved { return Err(device_model::network::NetworkError::WouldBlock); }
            if self.transmit.is_empty() { self.transmit.resize(1514, 0); }
            self.reserved = true;
            // SAFETY: 唯一 reservation，Vec 存储由 owner 保活；提交/取消前不 resize 或访问字节。
            Ok(unsafe { device_model::network::NetworkTransmit::new(owner, queue, 0, 1, self.transmit.as_mut_ptr() as usize, self.transmit.len()) })
        }
        fn complete(&mut self, length: usize) -> alloc::vec::Vec<u8> {
            assert!(self.reserved);
            self.reserved = false;
            self.transmit[..length].to_vec()
        }
    }
    struct Device;
    impl device_model::network::NetworkDevice for Device {
        fn queue_count(&self) -> u16 { 1 }
        fn bind_activity(&self, _: u16, _: device_model::Activity) {}
        fn mac_address(&self) -> [u8; 6] {
            [2, 0, 0, 0, 0, 1]
        }
        fn driver(&self) -> &str {
            "fixture"
        }
        fn link(
            &self,
        ) -> Result<device_model::network::NetworkLink, device_model::network::NetworkError> {
            Ok(device_model::network::NetworkLink::default())
        }
        fn receive(&self, queue: u16) -> Result<device_model::network::NetworkReceiveFrame<'_>, device_model::network::NetworkError> {
            let mut wire = WIRE.lock().unwrap();
            if wire.2.received.is_some() { return Err(device_model::network::NetworkError::WouldBlock); }
            let Some(frame) = wire.0.pop_front() else { return Err(device_model::network::NetworkError::WouldBlock); };
            Ok(wire.2.receive(self, queue, frame))
        }
        fn reserve_transmit(&self, queue: u16) -> Result<device_model::network::NetworkTransmit<'_>, device_model::network::NetworkError> {
            if !self.transmit_available(queue) { return Err(device_model::network::NetworkError::WouldBlock); }
            WIRE.lock().unwrap().2.reserve(self, queue)
        }
        unsafe fn release_received(&self, _: u16, _: u16, _: u64) -> Result<(), device_model::network::NetworkError> {
            assert!(WIRE.lock().unwrap().2.received.take().is_some());
            Ok(())
        }
        unsafe fn submit_transmit(&self, _: u16, _: u16, _: u64, length: usize) -> Result<(), device_model::network::NetworkError> {
            let mut wire = WIRE.lock().unwrap();
            let frame = wire.2.complete(length);
            wire.1.push(frame);
            Ok(())
        }
        unsafe fn cancel_transmit(&self, queue: u16, _: u16, _: u64) {
            WIRE.lock().unwrap().2.reserved = false;
            let _ = queue;
        }
        fn transmit_available(&self, _queue: u16) -> bool {
            TRANSMIT_AVAILABLE.load(core::sync::atomic::Ordering::Relaxed)
        }
        fn poll_completions(
            &self,
            _: u16,
            _: usize,
        ) -> Result<device_model::network::NetworkCompletion, device_model::network::NetworkError> {
            Ok(device_model::network::NetworkCompletion::default())
        }
        fn finish_receive_batch(&self, _queue: u16) -> Result<(), device_model::network::NetworkError> {
            Ok(())
        }
        fn statistics(&self) -> device_model::network::NetworkStatistics {
            device_model::network::NetworkStatistics::default()
        }
    }

// 仅编码边界 peer 的标准 wire frames；握手与 policy capture 不在 fixture 中实现。
pub(super) fn peer_tcp_frame(
    port: u16,
    control: smoltcp::wire::TcpControl,
    ack: Option<smoltcp::wire::TcpSeqNumber>,
) -> Vec<u8> {
    peer_tcp_payload(port, control, ack, 1001, &[])
}
pub(super) fn peer_tcp_payload(
    port: u16,
    control: smoltcp::wire::TcpControl,
    ack: Option<smoltcp::wire::TcpSeqNumber>,
    sequence: i32,
    payload: &[u8],
) -> Vec<u8> {
    use smoltcp::{phy::ChecksumCapabilities, wire::*};
    let source = Ipv4Addr::new(10, 0, 2, 16);
    let destination = Ipv4Addr::new(10, 0, 2, 15);
    let tcp = TcpRepr {
        src_port: 44000,
        dst_port: port,
        control,
        seq_number: TcpSeqNumber(if control == TcpControl::Syn {
            1000
        } else {
            sequence
        }),
        ack_number: ack,
        window_len: 32768,
        window_scale: None,
        max_seg_size: None,
        sack_permitted: false,
        sack_ranges: [None; 3],
        timestamp: None,
        payload,
    };
    let ip = Ipv4Repr {
        src_addr: source,
        dst_addr: destination,
        next_header: IpProtocol::Tcp,
        payload_len: tcp.buffer_len(),
        hop_limit: 64,
    };
    let mut bytes = vec![0; EthernetFrame::<&[u8]>::buffer_len(ip.buffer_len() + tcp.buffer_len())];
    let mut ethernet = EthernetFrame::new_unchecked(&mut bytes[..]);
    EthernetRepr {
        src_addr: EthernetAddress([2, 0, 0, 0, 0, 2]),
        dst_addr: EthernetAddress([2, 0, 0, 0, 0, 1]),
        ethertype: EthernetProtocol::Ipv4,
    }
    .emit(&mut ethernet);
    let mut packet = Ipv4Packet::new_unchecked(ethernet.payload_mut());
    ip.emit(&mut packet, &ChecksumCapabilities::default());
    tcp.emit(
        &mut TcpPacket::new_unchecked(packet.payload_mut()),
        &IpAddress::Ipv4(source),
        &IpAddress::Ipv4(destination),
        &ChecksumCapabilities::default(),
    );
    bytes
}
pub(super) fn peer_arp_frame() -> Vec<u8> {
    peer_arp_frame_for(Ipv4Addr::new(10, 0, 2, 15), Ipv4Addr::new(10, 0, 2, 16))
}
pub(super) fn peer_arp_frame_for(local: Ipv4Addr, peer: Ipv4Addr) -> Vec<u8> {
    use smoltcp::wire::*;
    let repr = ArpRepr::EthernetIpv4 {
        operation: ArpOperation::Request,
        source_hardware_addr: EthernetAddress([2, 0, 0, 0, 0, 2]),
        source_protocol_addr: peer,
        target_hardware_addr: EthernetAddress([0; 6]),
        target_protocol_addr: local,
    };
    let mut bytes = vec![0; EthernetFrame::<&[u8]>::buffer_len(repr.buffer_len())];
    let mut ethernet = EthernetFrame::new_unchecked(&mut bytes[..]);
    EthernetRepr {
        src_addr: EthernetAddress([2, 0, 0, 0, 0, 2]),
        dst_addr: EthernetAddress::BROADCAST,
        ethertype: EthernetProtocol::Arp,
    }
    .emit(&mut ethernet);
    repr.emit(&mut ArpPacket::new_unchecked(ethernet.payload_mut()));
    bytes
}
pub(super) fn ingress(frame: Vec<u8>) {
    WIRE.lock().unwrap().0.push_back(frame);
    namespace().protocol().unwrap().lock().unwrap().poll();
}
pub(super) fn syn_ack(port: u16) -> smoltcp::wire::TcpSeqNumber {
    use smoltcp::wire::*;
    let mut wire = WIRE.lock().unwrap();
    for frame in wire.1.drain(..) {
        let ethernet = EthernetFrame::new_checked(&frame[..]).unwrap();
        if ethernet.ethertype() != EthernetProtocol::Ipv4 {
            continue;
        }
        let ip = Ipv4Packet::new_checked(ethernet.payload()).unwrap();
        if ip.next_header() != IpProtocol::Tcp {
            continue;
        }
        let tcp = TcpPacket::new_checked(ip.payload()).unwrap();
        if tcp.src_port() == port && tcp.dst_port() == 44000 && tcp.syn() && tcp.ack() {
            return tcp.seq_number() + 1;
        }
    }
    panic!("actual protocol must emit SYN/ACK");
}
pub(super) fn address(port: u16) -> InetAddress {
    InetAddress {
        address: Ipv4Addr::UNSPECIFIED,
        port,
    }
}

/// 唯一边界 peer 的标准 UDP frame 编码；过滤/排队/半关闭仍执行真实 producer。
pub(super) fn peer_udp_frame(source: InetAddress, local_port: u16, payload: &[u8]) -> Vec<u8> {
    peer_udp_frame_for(
        source,
        InetAddress {
            address: Ipv4Addr::new(10, 0, 2, 15),
            port: local_port,
        },
        payload,
    )
}
pub(super) fn peer_udp_frame_for(
    source: InetAddress,
    local: InetAddress,
    payload: &[u8],
) -> Vec<u8> {
    use smoltcp::{phy::ChecksumCapabilities, wire::*};
    let destination = local.address;
    let local_port = local.port;
    let udp = UdpRepr {
        src_port: source.port,
        dst_port: local_port,
    };
    let ip = Ipv4Repr {
        src_addr: source.address,
        dst_addr: destination,
        next_header: IpProtocol::Udp,
        payload_len: udp.header_len() + payload.len(),
        hop_limit: 64,
    };
    let mut bytes = vec![0; EthernetFrame::<&[u8]>::buffer_len(ip.buffer_len() + ip.payload_len)];
    let mut ethernet = EthernetFrame::new_unchecked(&mut bytes[..]);
    EthernetRepr {
        src_addr: EthernetAddress([2, 0, 0, 0, 0, 2]),
        dst_addr: EthernetAddress([2, 0, 0, 0, 0, 1]),
        ethertype: EthernetProtocol::Ipv4,
    }
    .emit(&mut ethernet);
    let mut packet = Ipv4Packet::new_unchecked(ethernet.payload_mut());
    ip.emit(&mut packet, &ChecksumCapabilities::default());
    udp.emit(
        &mut UdpPacket::new_unchecked(packet.payload_mut()),
        &IpAddress::Ipv4(source.address),
        &IpAddress::Ipv4(destination),
        payload.len(),
        |output| output.copy_from_slice(payload),
        &ChecksumCapabilities::default(),
    );
    bytes
}

/// 标准ICMP echo reply边界输入；raw准入与PKTINFO由实际producer生成。
pub(super) fn peer_icmp_frame_for(source: Ipv4Addr, destination: Ipv4Addr) -> Vec<u8> {
    use smoltcp::{phy::ChecksumCapabilities, wire::*};
    let echo = Icmpv4Repr::EchoReply {
        ident: 1,
        seq_no: 2,
        data: b"raw-info",
    };
    let ip = Ipv4Repr {
        src_addr: source,
        dst_addr: destination,
        next_header: IpProtocol::Icmp,
        payload_len: echo.buffer_len(),
        hop_limit: 64,
    };
    let mut bytes = vec![0; EthernetFrame::<&[u8]>::buffer_len(ip.buffer_len() + ip.payload_len)];
    let mut ethernet = EthernetFrame::new_unchecked(&mut bytes[..]);
    EthernetRepr {
        src_addr: EthernetAddress([2, 0, 0, 0, 0, 2]),
        dst_addr: EthernetAddress([2, 0, 0, 0, 0, 1]),
        ethertype: EthernetProtocol::Ipv4,
    }
    .emit(&mut ethernet);
    let mut packet = Ipv4Packet::new_unchecked(ethernet.payload_mut());
    ip.emit(&mut packet, &ChecksumCapabilities::default());
    echo.emit(
        &mut Icmpv4Packet::new_unchecked(packet.payload_mut()),
        &ChecksumCapabilities::default(),
    );
    bytes
}

fn initial_stack() -> Option<&'static NetworkStackOwner> { namespace().protocol() }
fn counts() -> (usize, usize, usize) {
    let network = initial_stack().unwrap().lock().unwrap();
    (network.sockets.iter().count(), network.endpoints.len(), network.tcp_endpoints.len())
}

fn scoped_endpoint(scope: &Arc<crate::namespace::Namespace>, protocol: InetProtocol) -> Arc<InetSocket> {
    InetSocket::new(protocol, Arc::new(crate::Event::default()), scope.clone()).unwrap()
}
trait ReceiveEndpoint {
    type Receipt;
    fn receive_into(&self, buffer: &mut crate::receive::ReceiveBuffer, peek: bool) -> Result<Self::Receipt, SocketError>;
}
impl ReceiveEndpoint for InetSocket {
    type Receipt = (usize, usize, Option<InetAddress>, Option<InternetPacketInfo>);
    fn receive_into(&self, buffer: &mut crate::receive::ReceiveBuffer, peek: bool) -> Result<Self::Receipt, SocketError> { self.receive(buffer, peek) }
}
impl ReceiveEndpoint for crate::packet::PacketSocket {
    type Receipt = (usize, usize, network_protocol::PacketAddress);
    fn receive_into(&self, buffer: &mut crate::receive::ReceiveBuffer, peek: bool) -> Result<Self::Receipt, SocketError> { self.receive(buffer, peek) }
}
fn receive<E: ReceiveEndpoint>(endpoint: &Arc<E>, output: &mut [u8], peek: bool) -> Result<E::Receipt, SocketError> {
    let mut buffer = crate::receive::ReceiveBuffer::try_new(output.len()).unwrap();
    let result = endpoint.receive_into(&mut buffer, peek)?;
    let bytes = buffer.into_bytes();
    output[..bytes.len()].copy_from_slice(&bytes);
    Ok(result)
}

    pub(crate) struct WireDevice {
        pub(crate) queue_count: u16,
        pub(crate) receive_on: core::sync::atomic::AtomicU16,
        pub(crate) cancelled_queue: core::sync::atomic::AtomicU16,
        pub(crate) wire: std::sync::Mutex<WireQueues>,
        pub(crate) error: core::sync::atomic::AtomicBool,
        pub(crate) transmit_available: core::sync::atomic::AtomicBool,
        pub(crate) transmit_ready: core::sync::atomic::AtomicBool,
    }
    impl WireDevice {
        pub(crate) fn new() -> Self {
            Self {
                queue_count: 1,
                receive_on: core::sync::atomic::AtomicU16::new(0),
                cancelled_queue: core::sync::atomic::AtomicU16::new(u16::MAX),
                wire: std::sync::Mutex::new((
                    std::collections::VecDeque::new(),
                    alloc::vec::Vec::new(),
                    FrameLoans::new(),
                )),
                error: core::sync::atomic::AtomicBool::new(false),
                transmit_available: core::sync::atomic::AtomicBool::new(true),
                transmit_ready: core::sync::atomic::AtomicBool::new(false),
            }
        }
    }
    impl device_model::network::NetworkDevice for WireDevice {
        fn queue_count(&self) -> u16 { self.queue_count }
        fn bind_activity(&self, _: u16, _: device_model::Activity) {}
        fn mac_address(&self) -> [u8; 6] {
            [2, 0, 0, 0, 0, 1]
        }
        fn driver(&self) -> &str {
            "fixture"
        }
        fn link(
            &self,
        ) -> Result<device_model::network::NetworkLink, device_model::network::NetworkError> {
            Ok(device_model::network::NetworkLink::default())
        }
        fn receive(&self, queue: u16) -> Result<device_model::network::NetworkReceiveFrame<'_>, device_model::network::NetworkError> {
            if queue != self.receive_on.load(core::sync::atomic::Ordering::Relaxed) { return Err(device_model::network::NetworkError::WouldBlock); }
            if self.error.swap(false, core::sync::atomic::Ordering::Relaxed) { return Err(device_model::network::NetworkError::Device); }
            let mut wire = self.wire.lock().unwrap();
            if wire.2.received.is_some() { return Err(device_model::network::NetworkError::WouldBlock); }
            let Some(frame) = wire.0.pop_front() else { return Err(device_model::network::NetworkError::WouldBlock); };
            Ok(wire.2.receive(self, queue, frame))
        }
        fn reserve_transmit(&self, queue: u16) -> Result<device_model::network::NetworkTransmit<'_>, device_model::network::NetworkError> {
            if !self.transmit_available(queue) { return Err(device_model::network::NetworkError::WouldBlock); }
            self.wire.lock().unwrap().2.reserve(self, queue)
        }
        unsafe fn release_received(&self, _: u16, _: u16, _: u64) -> Result<(), device_model::network::NetworkError> {
            assert!(self.wire.lock().unwrap().2.received.take().is_some());
            Ok(())
        }
        unsafe fn submit_transmit(&self, _: u16, _: u16, _: u64, length: usize) -> Result<(), device_model::network::NetworkError> {
            let mut wire = self.wire.lock().unwrap();
            let frame = wire.2.complete(length);
            wire.1.push(frame);
            Ok(())
        }
        unsafe fn cancel_transmit(&self, queue: u16, _: u16, _: u64) {
            self.wire.lock().unwrap().2.reserved = false;
            self.cancelled_queue.store(queue, core::sync::atomic::Ordering::Relaxed);
        }
        fn transmit_available(&self, _queue: u16) -> bool {
            self.transmit_available
                .load(core::sync::atomic::Ordering::Relaxed)
        }
        fn poll_completions(
            &self,
            _: u16,
            _: usize,
        ) -> Result<device_model::network::NetworkCompletion, device_model::network::NetworkError> {
            Ok(device_model::network::NetworkCompletion {
                completed: 0,
                backlog: false,
                transmit_became_available: self
                    .transmit_ready
                    .swap(false, core::sync::atomic::Ordering::Relaxed),
            })
        }
        fn finish_receive_batch(&self, _queue: u16) -> Result<(), device_model::network::NetworkError> {
            Ok(())
        }
        fn statistics(&self) -> device_model::network::NetworkStatistics {
            device_model::network::NetworkStatistics::default()
        }
    }
