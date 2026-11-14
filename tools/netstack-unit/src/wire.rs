use std::sync::{Mutex as HostMutex, OnceLock};
static SERIAL: HostMutex<()> = HostMutex::new(());
static NAMESPACE: OnceLock<Arc<crate::namespace::Namespace>> = OnceLock::new();
fn namespace() -> &'static Arc<crate::namespace::Namespace> { NAMESPACE.get().unwrap() }
fn install_interface(
    scope: &crate::namespace::Namespace,
    device: Arc<dyn device_model::network::NetworkDevice>,
    lifetime: native_runtime::Channel,
    link_changed: native_runtime::Event,
) -> Result<u32, SocketError> {
    super::install_interface(scope, device, lifetime, link_changed, Default::default())
}
// 这个边界 peer 只发 IPv4；测试若错误传入 IPv6 必须显式失败，不能把地址截断后继续验收。
fn ipv4_address(endpoint: InetAddress) -> Ipv4Addr {
    let InetAddress::V4(endpoint) = endpoint else { panic!("IPv4 wire peer received an IPv6 endpoint"); };
    *endpoint.ip()
}
fn initialize() {
    let scope = NAMESPACE.get_or_init(|| {
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
        received: [Option<alloc::vec::Vec<u8>>; device_model::network::RECEIVE_BATCH_CAPACITY],
        transmit: alloc::vec::Vec<u8>,
        reserved: bool,
        // 边界故障输入：模拟在合法前缀之后拒绝 RX 记录，检查生产批 owner 的错误归还路径。
        receive_error_after: Option<usize>,
        // 一次实际 submit 故障，在同一个 wire owner 消费；不能用 reserve 失败替代。
        submit_error: Option<device_model::network::NetworkError>,
        receive_snapshot: Option<(u32, super::protocol_owner::ReceiveSnapshot)>,
        pub(crate) last_transmit_queue: Option<u16>,
    }
    impl FrameLoans {
        const fn new() -> Self { Self { received: [const { None }; device_model::network::RECEIVE_BATCH_CAPACITY], transmit: alloc::vec::Vec::new(), reserved: false, receive_error_after: None, submit_error: None, receive_snapshot: None, last_transmit_queue: None } }
        fn receive_batch<'a>(&mut self, owner: &'a dyn device_model::network::NetworkReceiveOwner, queue: u16, input: &mut std::collections::VecDeque<alloc::vec::Vec<u8>>, output: &mut [Option<device_model::network::NetworkReceiveFrame<'a>>], checksum_valid: bool) -> Result<usize, device_model::network::NetworkError> {
            if output.iter().any(Option::is_some) { return Err(device_model::network::NetworkError::InvalidConfiguration); }
            let mut count = 0;
            for output in output.iter_mut().take(device_model::network::RECEIVE_BATCH_CAPACITY) {
                if self.receive_error_after == Some(count) { return Err(device_model::network::NetworkError::Device); }
                let Some(slot) = self.received.iter().position(Option::is_none) else { break; };
                let Some(frame) = input.pop_front() else { break; };
                self.received[slot] = Some(frame);
                let frame = self.received[slot].as_ref().unwrap();
                // SAFETY: 每个 Vec 的独立存储由 owner 保活；本 slot 在对应 token 释放前不 take/resize/再次借出。
                *output = Some(unsafe {
                    let buffer = device_model::network::NetworkReceiveBuffer::prepare(frame, checksum_valid, |packet| {
                        self.receive_snapshot.as_ref().map_or_else(Default::default, |(ingress, snapshot)| snapshot.prepare(packet, true, *ingress))
                    });
                    device_model::network::NetworkReceiveFrame::new(owner, queue, slot as u16, 1, buffer)
                });
                count += 1;
            }
            Ok(count)
        }
        fn reserve<'a>(&mut self, owner: &'a dyn device_model::network::NetworkDevice, queue: u16) -> Result<device_model::network::NetworkTransmit<'a>, device_model::network::NetworkError> {
            if self.reserved { return Err(device_model::network::NetworkError::WouldBlock); }
            if self.transmit.is_empty() { self.transmit.resize(owner.transmit_capabilities().maximum_buffer_length as usize, 0); }
            // 重用 DMA 不承诺零值；整个实际 producer 必须定义提交窗口，不能依赖 adapter 清零。
            self.transmit.fill(0xa5);
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
    impl device_model::network::NetworkReceiveOwner for Device {
        // SAFETY: production token 已结束内容引用，take 是唯一持有帧存储的退休路径。
        unsafe fn release_received(&self, _: u16, slot: u16, _: u64) -> Result<(), device_model::network::NetworkError> {
            assert!(WIRE.lock().unwrap().2.received[slot as usize].take().is_some());
            Ok(())
        }
    }
    impl device_model::network::NetworkDevice for Device {
        fn supports_queue_affinity(&self) -> bool { false }
        fn set_queue_affinity(&self, _: u16, _: u32) -> Result<(), device_model::network::NetworkError> { Err(device_model::network::NetworkError::InvalidConfiguration) }
        fn queue_count(&self) -> u16 { 1 }
        fn transmit_capabilities(&self) -> device_model::network::NetworkTransmitCapabilities { device_model::network::NetworkTransmitCapabilities { checksum: true, tcp_ipv4: false, tcp_ipv6: false, tcp_ecn: false, maximum_frame_length: 1514, maximum_buffer_length: 1514 } }
        fn bind_activity(&self, _: u16, _: device_model::Activity) {}
        fn bind_link_activity(&self, activity: device_model::Activity) { activity(); }
        fn mac_address(&self) -> [u8; 6] {
            [2, 0, 0, 0, 0, 1]
        }
        fn driver(&self) -> &str {
            "fixture"
        }
        fn link(
            &self,
        ) -> Result<device_model::network::NetworkLink, device_model::network::NetworkError> {
            Ok(device_model::network::NetworkLink {
                carrier: true,
                ..Default::default()
            })
        }
        fn receive_batch<'a>(
            &'a self,
            start: u16,
            output: &mut [Option<device_model::network::NetworkReceiveFrame<'a>>],
        ) -> Result<Option<(u16, usize)>, device_model::network::NetworkError> {
            if start != 0 {
                return Err(device_model::network::NetworkError::InvalidConfiguration);
            }
            let mut wire = WIRE.lock().unwrap();
            let (input, _, loans) = &mut *wire;
            let count = loans.receive_batch(self, 0, input, output, false)?;
            Ok((count != 0).then_some((0, count)))
        }
        fn reserve_transmit(&self, queue: u16) -> Result<device_model::network::NetworkTransmit<'_>, device_model::network::NetworkError> {
            if !self.transmit_available(queue) { return Err(device_model::network::NetworkError::WouldBlock); }
            WIRE.lock().unwrap().2.reserve(self, queue)
        }
        // SAFETY: production token 已交出写入权；边界 peer 此时才读取并记录帧，随后立即完成。
        unsafe fn submit_transmit(&self, _: u16, _: u16, _: u64, length: usize, data: device_model::network::NetworkTransmitData) -> Result<(), device_model::network::NetworkError> {
            let device_model::network::NetworkTransmitData::Ready(checksum, segmentation) = data else {
                return Err(device_model::network::NetworkError::InvalidConfiguration);
            };
            assert!(segmentation.is_none());
            let mut wire = WIRE.lock().unwrap();
            let mut frame = wire.2.complete(length);
            if let Some(checksum) = checksum {
                checksum.validate(frame.len())?;
                // 只模拟 NIC 的 ones-complement 完成，不重写协议状态；输入已含实际 producer 的 seed。
                let value = packet_wire::checksum::checksum(&frame[usize::from(checksum.start)..]);
                let value = if value == 0 { u16::MAX } else { value };
                let field = usize::from(checksum.start) + usize::from(checksum.offset);
                frame[field..field + 2].copy_from_slice(&value.to_be_bytes());
            }
            wire.1.push(frame);
            Ok(())
        }
        // SAFETY: production token 只在尚未提交时取消，内容引用已结束，无硬件异步访问。
        unsafe fn cancel_transmit(&self, queue: u16, _: u16, _: u64) {
            WIRE.lock().unwrap().2.reserved = false;
            let _ = queue;
        }
        fn transmit_available(&self, _queue: u16) -> bool {
            TRANSMIT_AVAILABLE.load(core::sync::atomic::Ordering::Relaxed)
        }
        fn take_progress(&self) -> device_model::network::NetworkDeviceProgress {
            device_model::network::NetworkDeviceProgress::default()
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
        assert!(tcp.verify_checksum(&ip.src_addr().into(), &ip.dst_addr().into()), "actual emitted TCP checksum");
        if tcp.src_port() == port && tcp.dst_port() == 44000 && tcp.syn() && tcp.ack() {
            return tcp.seq_number() + 1;
        }
    }
    panic!("actual protocol must emit SYN/ACK");
}
pub(super) fn address(port: u16) -> InetAddress {
    InetAddress::V4(core::net::SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port))
}

/// 唯一边界 peer 的标准 UDP frame 编码；过滤/排队/半关闭仍执行真实 producer。
pub(super) fn peer_udp_frame(source: InetAddress, local_port: u16, payload: &[u8]) -> Vec<u8> {
    peer_udp_frame_for(
        source,
        InetAddress::V4(core::net::SocketAddrV4::new(Ipv4Addr::new(10, 0, 2, 15), local_port)),
        payload,
    )
}
pub(super) fn peer_udp_frame_for(
    source: InetAddress,
    local: InetAddress,
    payload: &[u8],
) -> Vec<u8> {
    use smoltcp::{phy::ChecksumCapabilities, wire::*};
    let destination = ipv4_address(local);
    let local_port = local.port();
    let udp = UdpRepr {
        src_port: source.port(),
        dst_port: local_port,
    };
    let ip = Ipv4Repr {
        src_addr: ipv4_address(source),
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
        &IpAddress::Ipv4(ipv4_address(source)),
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
    (network.socket_count() + network.udp_endpoints.len(), network.udp_endpoints.len(), network.tcp_endpoints.len())
}

fn scoped_endpoint(scope: &Arc<crate::namespace::Namespace>, protocol: InetProtocol) -> Arc<InetSocket> {
    InetSocket::new(network_protocol::InetFamily::Ipv4, protocol, Arc::new(crate::Event::default()), scope.clone()).unwrap()
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
    type Receipt = (usize, usize, network_protocol::PacketAddress, network_protocol::PacketInformation);
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
        link: std::sync::Mutex<(bool, u32, Option<device_model::Activity>)>,
        pub(crate) link_reads: core::sync::atomic::AtomicUsize,
        pub(crate) queue_count: u16,
        pub(crate) checksum: bool,
        pub(crate) receive_checksum_valid: core::sync::atomic::AtomicBool,
        pub(crate) receive_on: core::sync::atomic::AtomicU16,
        pub(crate) cancelled_queue: core::sync::atomic::AtomicU16,
        pub(crate) wire: std::sync::Mutex<WireQueues>,
        pub(crate) error: core::sync::atomic::AtomicBool,
        pub(crate) transmit_available: core::sync::atomic::AtomicBool,
        pub(crate) transmit_ready: core::sync::atomic::AtomicBool,
    }
    impl WireDevice {
        pub(crate) fn set_carrier(&self, carrier: bool) {
            let callback = {
                let mut link = self.link.lock().unwrap();
                if link.0 != carrier {
                    link.1 = link.1.wrapping_add(1);
                }
                link.0 = carrier;
                link.2.clone()
            };
            if let Some(callback) = callback { callback(); }
        }
        pub(crate) fn new() -> Self {
            Self {
                link: std::sync::Mutex::new((true, 0, None)),
                link_reads: core::sync::atomic::AtomicUsize::new(0),
                queue_count: 1,
                checksum: false,
                receive_checksum_valid: core::sync::atomic::AtomicBool::new(false),
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
    impl device_model::network::NetworkReceiveOwner for WireDevice {
        // SAFETY: production token 已结束内容引用，take 是唯一持有帧存储的退休路径。
        unsafe fn release_received(&self, _: u16, slot: u16, _: u64) -> Result<(), device_model::network::NetworkError> {
            assert!(self.wire.lock().unwrap().2.received[slot as usize].take().is_some());
            Ok(())
        }
    }
    impl device_model::network::NetworkDevice for WireDevice {
        fn supports_queue_affinity(&self) -> bool { false }
        fn set_queue_affinity(&self, _: u16, _: u32) -> Result<(), device_model::network::NetworkError> { Err(device_model::network::NetworkError::InvalidConfiguration) }
        fn queue_count(&self) -> u16 { self.queue_count }
        fn transmit_capabilities(&self) -> device_model::network::NetworkTransmitCapabilities { device_model::network::NetworkTransmitCapabilities { checksum: self.checksum, tcp_ipv4: false, tcp_ipv6: false, tcp_ecn: false, maximum_frame_length: 1518, maximum_buffer_length: 1518 } }
        fn bind_activity(&self, _: u16, _: device_model::Activity) {}
        fn bind_link_activity(&self, activity: device_model::Activity) {
            let old = self.link.lock().unwrap().2.replace(activity.clone());
            drop(old);
            activity();
        }
        fn mac_address(&self) -> [u8; 6] {
            [2, 0, 0, 0, 0, 1]
        }
        fn driver(&self) -> &str {
            "fixture"
        }
        fn link(
            &self,
        ) -> Result<device_model::network::NetworkLink, device_model::network::NetworkError> {
            self.link_reads.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            let link = self.link.lock().unwrap();
            Ok(device_model::network::NetworkLink {
                carrier: link.0,
                carrier_epoch: link.1,
                speed_mbps: None,
            })
        }
        fn receive_batch<'a>(
            &'a self,
            start: u16,
            output: &mut [Option<device_model::network::NetworkReceiveFrame<'a>>],
        ) -> Result<Option<(u16, usize)>, device_model::network::NetworkError> {
            if start >= self.queue_count {
                return Err(device_model::network::NetworkError::InvalidConfiguration);
            }
            // 该 fixture 同时只给一个队列送帧，从任一起点循环搜索都会选中它。
            let queue = self.receive_on.load(core::sync::atomic::Ordering::Relaxed);
            if queue >= self.queue_count {
                return Ok(None);
            }
            if self.error.swap(false, core::sync::atomic::Ordering::Relaxed) {
                return Err(device_model::network::NetworkError::Device);
            }
            let mut wire = self.wire.lock().unwrap();
            let (input, _, loans) = &mut *wire;
            let count = loans.receive_batch(
                self,
                queue,
                input,
                output,
                self.receive_checksum_valid.load(core::sync::atomic::Ordering::Relaxed),
            )?;
            Ok((count != 0).then_some((queue, count)))
        }
        fn reserve_transmit(&self, queue: u16) -> Result<device_model::network::NetworkTransmit<'_>, device_model::network::NetworkError> {
            if !self.transmit_available(queue) { return Err(device_model::network::NetworkError::WouldBlock); }
            self.wire.lock().unwrap().2.reserve(self, queue)
        }
        // SAFETY: production token 已交出写入权；边界 peer 此时才读取并记录帧，随后立即完成。
        unsafe fn submit_transmit(&self, queue: u16, _: u16, _: u64, length: usize, data: device_model::network::NetworkTransmitData) -> Result<(), device_model::network::NetworkError> {
            let device_model::network::NetworkTransmitData::Ready(checksum, segmentation) = data else {
                return Err(device_model::network::NetworkError::InvalidConfiguration);
            };
            assert!(segmentation.is_none());
            let mut wire = self.wire.lock().unwrap();
            if let Some(error) = wire.2.submit_error.take() { return Err(error); }
            let mut frame = wire.2.complete(length);
            if let Some(checksum) = checksum {
                checksum.validate(frame.len())?;
                // 只模拟 NIC 的 ones-complement 完成，不重写协议状态；输入已含实际 producer 的 seed。
                let value = packet_wire::checksum::checksum(&frame[usize::from(checksum.start)..]);
                let value = if value == 0 { u16::MAX } else { value };
                let field = usize::from(checksum.start) + usize::from(checksum.offset);
                frame[field..field + 2].copy_from_slice(&value.to_be_bytes());
            }
            wire.1.push(frame);
            wire.2.last_transmit_queue = Some(queue);
            Ok(())
        }
        // SAFETY: production token 只在尚未提交时取消，内容引用已结束，无硬件异步访问。
        unsafe fn cancel_transmit(&self, queue: u16, _: u16, _: u64) {
            self.wire.lock().unwrap().2.reserved = false;
            self.cancelled_queue.store(queue, core::sync::atomic::Ordering::Relaxed);
        }
        fn transmit_available(&self, _queue: u16) -> bool {
            self.transmit_available
                .load(core::sync::atomic::Ordering::Relaxed)
        }
        fn take_progress(&self) -> device_model::network::NetworkDeviceProgress {
            device_model::network::NetworkDeviceProgress {
                transmit_ready: self.transmit_ready.swap(false, core::sync::atomic::Ordering::Relaxed),
                ..Default::default()
            }
        }
        fn statistics(&self) -> device_model::network::NetworkStatistics {
            device_model::network::NetworkStatistics::default()
        }
    }

// 配置边界夹具只编码实际协议，再调用唯一生产预备入口，不复制事务或路由准入算法。
fn prepare_configuration<'a>(scope: &Arc<crate::namespace::Namespace>, plans: impl ExactSizeIterator<Item = router_protocol::interface::InterfaceConfiguration<'a>>, forwarding: Option<network_protocol::Forwarding>) -> Result<super::configuration::ConfigurationSession, SocketError> {
    let mut rows = Vec::new();
    rows.try_reserve_exact(plans.len()).map_err(|_| SocketError::NoMemory)?;
    rows.extend(plans);
    if rows.is_empty() { return super::prepare_configuration(scope, None, forwarding); }
    let bytes = network_protocol::ConfigurationBatch::encode(&rows).map_err(configuration_input_error)?;
    let batch = network_protocol::ConfigurationBatch::parse(&bytes).map_err(configuration_input_error)?;
    super::prepare_configuration(scope, Some(batch), forwarding)
}
fn configuration_input_error(status: native_abi::Status) -> SocketError {
    if status == native_abi::ERR_NO_MEMORY { SocketError::NoMemory } else { SocketError::Invalid }
}
fn prepare_routes<'a, 'h>(scope: &Arc<crate::namespace::Namespace>, plans: impl ExactSizeIterator<Item = router_protocol::interface::InterfaceConfiguration<'a>>, hops: impl Clone + ExactSizeIterator<Item = network_protocol::routes::NextHopConfiguration<'h, Ipv4Addr>>, routes: impl Clone + ExactSizeIterator<Item = network_protocol::routes::RouteConfiguration<Ipv4Addr>>, forwarding: Option<network_protocol::Forwarding>) -> Result<super::configuration::ConfigurationSession, SocketError> {
    let mut rows = Vec::new();
    rows.try_reserve_exact(plans.len()).map_err(|_| SocketError::NoMemory)?;
    rows.extend(plans);
    let bytes = network_protocol::ConfigurationBatch::encode_forwarding(&rows, hops, routes, core::iter::empty(), core::iter::empty(), &[], None).map_err(configuration_input_error)?;
    let batch = network_protocol::ConfigurationBatch::parse(&bytes).map_err(configuration_input_error)?;
    super::prepare_configuration(scope, Some(batch), forwarding)
}

// RIB 输入来自同一实际 NET owner，不能用旧 FIB 或空地址集合替代 IPv6 配置事实。
fn routing_facts(scope: &crate::namespace::Namespace) -> routing_rib::sources::Facts {
    routing_rib::sources::Facts {
        interfaces: interface_snapshots(scope).unwrap(),
        addresses: address_snapshot(scope, 0).unwrap(),
        neighbors: crate::inet::configuration::neighbor_snapshot(scope, 0).unwrap(),
    }
}
