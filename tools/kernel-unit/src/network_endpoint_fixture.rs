// 实际 packet/netlink/INET owner；Ethernet adapter 与 facade domain values 是边界。
macro_rules! error { ($($message:tt)*) => {{ eprintln!($($message)*); }}; }
// 唯一 deferred delivery adapter；只记录真实 driver 通知，不复制轮询/协议算法。
pub(crate) mod deferred {
    pub(crate) struct DeferredWork;
    impl DeferredWork { pub(crate) const NETWORK: Self = Self; }
    pub(crate) fn raise(_: DeferredWork) {
        crate::drivers::network::POLL_REQUESTS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    }
}
pub(crate) mod timer {
    pub(crate) const SCHEDULER_TICK_NS: u64 = 10_000_000;
    pub(crate) fn get_time_us() -> u64 {
        static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
        START
            .get_or_init(std::time::Instant::now)
            .elapsed()
            .as_micros() as u64
    }
}
#[path = "allocation_failure.rs"]
pub(crate) mod allocation_failure;
#[path = "../../../kernel/src/fallible_tree/mod.rs"]
pub(crate) mod fallible_tree;
#[path = "../../../kernel/src/id.rs"]
pub(crate) mod id;
#[path = "pipe_fixture.rs"]
pub(crate) mod ipc;
#[path = "namespace_fixture.rs"]
pub(crate) mod namespace;
#[path = "sync.rs"]
pub(crate) mod sync;
pub(crate) mod drivers {
    type WireQueues = (std::collections::VecDeque<alloc::vec::Vec<u8>>, alloc::vec::Vec<alloc::vec::Vec<u8>>);
    // 唯一边界 NIC 保存 wire frames；协议解析、握手和继承全部执行 production owners。
    pub(crate) static WIRE: std::sync::Mutex<WireQueues> = std::sync::Mutex::new((std::collections::VecDeque::new(), alloc::vec::Vec::new()));
    // NIC capacity 是边界输入；RST 重试和实际 handle 退休仍由 production owner 决定。
    pub(crate) static TRANSMIT_AVAILABLE: core::sync::atomic::AtomicBool =
        core::sync::atomic::AtomicBool::new(true);
    #[allow(clippy::items_after_test_module, reason = "actual driver leaf owns internal tests; host notification adapter follows the include")]
    pub(crate) mod network {
        include!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../kernel/src/drivers/network.rs"
        ));
        // deferred delivery 为边界；真实 protocol cleanup/ring 仍由 production owner 执行。
        pub(crate) static POLL_REQUESTS: core::sync::atomic::AtomicUsize =
            core::sync::atomic::AtomicUsize::new(0);
        #[cfg(test)]
        pub(crate) fn request_poll() {
            crate::deferred::raise(crate::deferred::DeferredWork::NETWORK);
        }
    }
    use alloc::sync::Arc;
    struct Device;
    impl network::NetworkDevice for Device {
        fn mac_address(&self) -> [u8; 6] {
            [2, 0, 0, 0, 0, 1]
        }
        fn driver(&self) -> &'static str {
            "fixture"
        }
        fn receive(&self, output: &mut [u8]) -> Result<usize, network::NetworkError> {
            let Some(frame) = WIRE.lock().unwrap().0.pop_front() else {
                return Err(network::NetworkError::WouldBlock);
            };
            assert!(frame.len() <= output.len());
            output[..frame.len()].copy_from_slice(&frame);
            Ok(frame.len())
        }
        fn reserve_transmit(&self) -> Result<u16, network::NetworkError> {
            if self.transmit_available() {
                Ok(0)
            } else {
                Err(network::NetworkError::WouldBlock)
            }
        }
        fn submit_transmit(&self, _: u16, bytes: &[u8]) -> Result<(), network::NetworkError> {
            WIRE.lock().unwrap().1.push(bytes.to_vec());
            Ok(())
        }
        fn cancel_transmit(&self, _: u16) {}
        fn transmit_available(&self) -> bool {
            TRANSMIT_AVAILABLE.load(core::sync::atomic::Ordering::Relaxed)
        }
        fn poll_completions(
            &self,
            _: usize,
        ) -> Result<network::NetworkCompletion, network::NetworkError> {
            Ok(network::NetworkCompletion::default())
        }
        fn finish_receive_batch(&self) -> Result<(), network::NetworkError> {
            Ok(())
        }
        fn statistics(&self) -> network::NetworkStatistics {
            network::NetworkStatistics::default()
        }
    }
    pub(crate) fn network_device(index: usize) -> Option<Arc<dyn network::NetworkDevice>> {
        (index == 0).then(|| Arc::new(Device) as Arc<dyn network::NetworkDevice>)
    }
    /// 独立wire输入NIC；不解析/路由任何协议，不与boot NIC共享帧或故障。
    pub(crate) struct WireDevice {
        pub(crate) wire: std::sync::Mutex<WireQueues>,
        pub(crate) error: core::sync::atomic::AtomicBool,
        pub(crate) transmit_available: core::sync::atomic::AtomicBool,
        pub(crate) transmit_ready: core::sync::atomic::AtomicBool,
    }
    impl WireDevice {
        pub(crate) fn new() -> Self {
            Self {
                wire: std::sync::Mutex::new((
                    std::collections::VecDeque::new(),
                    alloc::vec::Vec::new(),
                )),
                error: core::sync::atomic::AtomicBool::new(false),
                transmit_available: core::sync::atomic::AtomicBool::new(true),
                transmit_ready: core::sync::atomic::AtomicBool::new(false),
            }
        }
    }
    impl network::NetworkDevice for WireDevice {
        fn mac_address(&self) -> [u8; 6] {
            [2, 0, 0, 0, 0, 1]
        }
        fn driver(&self) -> &'static str {
            "fixture"
        }
        fn receive(&self, output: &mut [u8]) -> Result<usize, network::NetworkError> {
            if self
                .error
                .swap(false, core::sync::atomic::Ordering::Relaxed)
            {
                return Err(network::NetworkError::Device);
            }
            let Some(frame) = self.wire.lock().unwrap().0.pop_front() else {
                return Err(network::NetworkError::WouldBlock);
            };
            assert!(frame.len() <= output.len());
            output[..frame.len()].copy_from_slice(&frame);
            Ok(frame.len())
        }
        fn reserve_transmit(&self) -> Result<u16, network::NetworkError> {
            if self
                .transmit_available
                .load(core::sync::atomic::Ordering::Relaxed)
            {
                Ok(0)
            } else {
                Err(network::NetworkError::WouldBlock)
            }
        }
        fn submit_transmit(&self, _: u16, frame: &[u8]) -> Result<(), network::NetworkError> {
            self.wire.lock().unwrap().1.push(frame.to_vec());
            Ok(())
        }
        fn cancel_transmit(&self, _: u16) {}
        fn transmit_available(&self) -> bool {
            self.transmit_available
                .load(core::sync::atomic::Ordering::Relaxed)
        }
        fn poll_completions(
            &self,
            _: usize,
        ) -> Result<network::NetworkCompletion, network::NetworkError> {
            Ok(network::NetworkCompletion {
                backlog: false,
                transmit_became_available: self
                    .transmit_ready
                    .swap(false, core::sync::atomic::Ordering::Relaxed),
            })
        }
        fn finish_receive_batch(&self) -> Result<(), network::NetworkError> {
            Ok(())
        }
        fn statistics(&self) -> network::NetworkStatistics {
            network::NetworkStatistics::default()
        }
    }
}
#[path = "../../../kernel/src/socket"]
pub(crate) mod socket {
    use alloc::sync::Arc;
    // OWNER: 本 fixture 内所有 socket 测试共用的串行锁。这些测试都会重置同一套进程级全局状态
    // （namespace::boot、initialize_net、packet/inet registry）；若每个测试模块各持私有锁，模块
    // 内部串行而模块之间并行，会互相覆盖全局状态，表现为随机的 poll/bind/queue 断言失败。
    #[cfg(test)]
    pub(crate) static NETWORK_TEST_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    #[path = "net_namespace.rs"]
    mod net_namespace;
    pub(crate) use inet::{InternetOption, create_net_namespace};
    pub(crate) use net_namespace::{NetNamespace, initial_net, initialize_net};
    mod core_buffer_limit;
    mod core_buffers;
    mod unix {
        // 全局sysctl minimum由actual UNIX fixture覆盖；此binary不调用该布局边界。
        pub(super) fn minimum_buffer(_: super::SocketIoDirection) -> usize {
            unreachable!("actual UNIX packet layout covered by unix_endpoint_fixture")
        }
    }
    pub(crate) use core_buffer_limit::CoreBufferLimit;
    mod control_memory;
    pub(crate) use control_memory::{ControlMemoryCharge, SocketMemory};
    mod timeouts;
    use timeouts::SocketTimeouts;
    pub(crate) use timeouts::{SocketIoDirection, SocketTimeout};
    pub(crate) mod inet {
        include!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tools/kernel-view/socket/inet/inet/inet.rs"
        ));
        // fixture composition的显式initial入口，不存在于production IO/deferred代码。
        fn initial_stack() -> Option<&'static NetworkStackOwner> {
            super::net_namespace::initial_net_ref().protocol()
        }
        mod wire_inputs {
            use super::*;
            include!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../tools/kernel-unit/src/inet_wire_inputs.rs"
            ));
        }
        pub(crate) fn lifecycle_benchmark(
            mut measure: impl FnMut(&str, u64, usize, bool, &mut dyn FnMut()),
        ) {
            use crate::ipc::Pipe;
            use smoltcp::wire::TcpControl;
            wire_inputs::initialize();
            {
                let mut network = initial_stack().unwrap().lock().unwrap();
                for number in 0..2048u32 {
                    let destination = Ipv4Addr::from(0xac10_0000 + number);
                    let request = route::Ipv4RouteRequest {
                        destination,
                        prefix_length: 32,
                        table: 254,
                        metric: 1,
                        scope: 253,
                        protocol: 3,
                        kind: route::Ipv4RouteKind::Unicast,
                        interface_index: Some(2),
                        gateway: None,
                        preferred_source: None,
                        metrics: route::Ipv4RouteMetrics::default(),
                        on_link: false,
                    };
                    let NetworkStack {
                        routing,
                        interfaces,
                        ..
                    } = &mut *network;
                    routing
                        .insert(request, interfaces, false, true, false, false)
                        .unwrap();
                }
                let destination = Ipv4Addr::from(0xac10_0000 + 1023);
                measure(
                    "actual indexed IPv4 FIB lookup, 2048 routes",
                    500_000,
                    1,
                    true,
                    &mut || {
                        std::hint::black_box(
                            network
                                .route(std::hint::black_box(destination), None)
                                .unwrap(),
                        );
                    },
                );
            }
            let listener = InetSocket::new(
                InetProtocol::Tcp,
                Pipe::notification_pair().unwrap(),
                Arc::new(SocketTimeouts::new()),
                crate::socket::SocketMemory::try_new(crate::socket::initial_net()).unwrap(),
            )
            .unwrap();
            listener.bind(wire_inputs::address(43010)).unwrap();
            listener.listen(1).unwrap();
            wire_inputs::ingress(wire_inputs::peer_arp_frame());
            wire_inputs::ingress(wire_inputs::peer_tcp_frame(43010, TcpControl::Syn, None));
            measure(
                "actual TCP standard option query",
                200_000,
                5,
                false,
                &mut || {
                    std::hint::black_box(listener.option(super::InternetOption::NoDelay).unwrap());
                },
            );
            let ack = wire_inputs::syn_ack(43010);
            wire_inputs::ingress(wire_inputs::peer_tcp_frame(
                43010,
                TcpControl::None,
                Some(ack),
            ));
            let reset_frame = wire_inputs::peer_tcp_frame(43010, TcpControl::Rst, Some(ack));
            let mut network = initial_stack().unwrap().lock().unwrap();
            measure(
                "actual pending TCP lifecycle observation",
                2_000_000,
                1,
                true,
                &mut || tcp::maintain(&mut network, None),
            );
            drop(network);
            // 仅 boundary producer 提供重复 wire frame；parser 与 protocol poll 均为实际代码。
            measure(
                "actual TCP wire reset and NIC poll",
                20_000,
                1,
                false,
                &mut || wire_inputs::ingress(reset_frame.clone()),
            );
            listener.close_description();
            initial_stack().unwrap().lock().unwrap().poll();
            let mut endpoints = Vec::new();
            for _ in 0..SOCKET_STORAGE_CAPACITY {
                endpoints.push(
                    InetSocket::new(
                        InetProtocol::Tcp,
                        Pipe::notification_pair().unwrap(),
                        Arc::new(SocketTimeouts::new()),
                        crate::socket::SocketMemory::try_new(crate::socket::initial_net()).unwrap(),
                    )
                    .unwrap(),
                );
            }
            let mut network = initial_stack().unwrap().lock().unwrap();
            measure(
                "actual bounded TCP lifecycle scan per handle",
                20_000,
                SOCKET_STORAGE_CAPACITY,
                true,
                &mut || tcp::maintain(&mut network, None),
            );
            let clock_time = now();
            measure(
                "actual route-independent TCP clock phase per handle",
                20_000,
                SOCKET_STORAGE_CAPACITY,
                true,
                &mut || tcp::maintain_clocks(&mut network, clock_time),
            );
            measure(
                "actual complete single-NIC/loopback NET poll per handle",
                20_000,
                SOCKET_STORAGE_CAPACITY,
                true,
                &mut || {
                    std::hint::black_box(network.poll_at(clock_time));
                },
            );
            drop(network);
            for endpoint in endpoints {
                endpoint.close_description();
            }
            initial_stack().unwrap().lock().unwrap().poll();
            let datagram = InetSocket::new(
                InetProtocol::Udp,
                Pipe::notification_pair().unwrap(),
                Arc::new(SocketTimeouts::new()),
                crate::socket::SocketMemory::try_new(crate::socket::initial_net()).unwrap(),
            )
            .unwrap();
            datagram.bind(wire_inputs::address(43110)).unwrap();
            datagram
                .connect(InetAddress {
                    address: Ipv4Addr::new(10, 0, 2, 16),
                    port: 44000,
                })
                .unwrap();
            measure(
                "actual UDP native readiness",
                2_000_000,
                1,
                true,
                &mut || {
                    std::hint::black_box(datagram.poll_state());
                },
            );
            let rejected = wire_inputs::peer_udp_frame(
                InetAddress {
                    address: Ipv4Addr::new(10, 0, 2, 16),
                    port: 44001,
                },
                43110,
                b"rejected",
            );
            measure(
                "actual UDP ingress peer admission and NIC poll",
                20_000,
                1,
                false,
                &mut || wire_inputs::ingress(rejected.clone()),
            );
            datagram.shutdown(0).unwrap();
            let mut byte = [0u8; 1];
            measure(
                "actual UDP payload loan and native EOF",
                100_000,
                1,
                false,
                &mut || {
                    std::hint::black_box(
                        datagram
                            .receive(&mut crate::ipc::ReceiveBuffer::from_slice(&mut byte), false)
                            .unwrap(),
                    );
                },
            );
            datagram.close_description();
            initial_stack().unwrap().lock().unwrap().poll();
            let endpoint = super::kobject::KobjectSocket::new(
                Pipe::notification_pair().unwrap(),
                crate::socket::SocketMemory::try_new(crate::socket::initial_net()).unwrap(),
            )
            .unwrap();
            let address = super::NetlinkAddress {
                port_id: 9026,
                groups: 1,
            };
            endpoint.bind(address, 9026).unwrap();
            measure(
                "actual netlink scoped no-allocation rebind",
                100_000,
                1,
                false,
                &mut || {
                    endpoint.bind(address, 9026).unwrap();
                },
            );
            let mut event = [0; 256];
            measure(
                "actual netlink broadcast FIFO allocation and receive",
                50_000,
                1,
                false,
                &mut || {
                    super::kobject::publish_drm_hotplug();
                    std::hint::black_box(
                        endpoint
                            .receive(
                                &mut crate::ipc::ReceiveBuffer::from_slice(&mut event),
                                false,
                            )
                            .unwrap(),
                    );
                },
            );
            endpoint.close_description();
        }
        #[cfg(test)]
        mod lifecycle_tests {
            use super::wire_inputs::*;
            use super::*;
            use crate::ipc::Pipe;
            include!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../tools/kernel-unit/tests/inet_endpoint_lifecycle.rs.inc"
            ));
            include!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../tools/kernel-unit/tests/udp_endpoint_lifecycle.rs.inc"
            ));
            include!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../tools/kernel-unit/tests/inet_network_domains.rs.inc"
            ));
            include!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../tools/kernel-unit/tests/ipv4_routing.rs.inc"
            ));
        }
    }
    mod kobject;
    mod message_limits;
    mod packet;
    #[derive(Clone, Copy)]
    pub(crate) enum SocketDomain { Unix, Inet, Packet, Netlink }
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) enum SocketType {
        Stream,
        Datagram,
        SeqPacket,
        Raw,
    }
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) struct InetAddress {
        pub(crate) address: core::net::Ipv4Addr,
        pub(crate) port: u16,
    }
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) struct PacketAddress {
        pub(crate) protocol: u16,
        pub(crate) interface_index: i32,
        pub(crate) hardware_type: u16,
        pub(crate) packet_type: u8,
        pub(crate) address_length: u8,
        pub(crate) address: [u8; 8],
    }
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) struct NetlinkAddress {
        pub(crate) port_id: u32,
        pub(crate) groups: u32,
    }
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum SocketAddress {
        Netlink(NetlinkAddress),
    }
    /// sendmsg单次IPv4源地址/出口选择；只随请求持有，不改变endpoint binding。
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub(crate) struct InternetSendPacketInfo {
        pub(crate) interface_index: Option<u32>,
        pub(crate) source_address: Option<core::net::Ipv4Addr>,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) struct InternetPacketInfo {
        pub(crate) interface_index: u32,
        pub(crate) specific_address: core::net::Ipv4Addr,
        pub(crate) destination: core::net::Ipv4Addr,
    }
    pub(crate) struct ReceivedMessage {
        pub(crate) count: usize,
        pub(crate) full_length: usize,
        pub(crate) source: Option<SocketAddress>,
        pub(crate) internet_packet_info: Option<InternetPacketInfo>,
        pub(crate) rights: Option<()>,
        pub(crate) credentials: Option<()>,
    }
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum SocketError {
        NetworkUnreachable,
        HostUnreachable,
        ProtocolNotSupported,
        NoMemory,
        NoSpace,
        NoBuffer,
        AddressNotAvailable,
        NotConnected,
        Invalid,
        MessageTooLarge,
        DestinationRequired,
        Again,
        AddressInUse,
        Device,
        AlreadyConnected,
        AlreadyInProgress,
        ConnectionRefused,
        ConnectionReset,
        ConnectionAborted,
        TimedOut,
        InProgress,
        OperationNotSupported,
        WrongType,
        BrokenPipe,
        PermissionDenied,
        OperationNotPermitted,
        NoDevice,
        DeviceNotAvailable,
        NetworkDown,
    }
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) struct SocketPollState {
        pub(crate) readable: bool,
        pub(crate) writable: bool,
        pub(crate) hangup: bool,
        pub(crate) read_closed: bool,
        pub(crate) error: bool,
    }
    impl SocketPollState {
        pub(crate) fn closed() -> Self {
            Self {
                readable: true,
                writable: false,
                hangup: true,
                read_closed: true,
                error: false,
            }
        }
        pub(crate) fn error() -> Self {
            Self {
                readable: false,
                writable: false,
                hangup: false,
                read_closed: false,
                error: true,
            }
        }
    }
    mod wait_source;
    pub(crate) use wait_source::SocketWaitSource;
    pub(crate) type SocketWaitSources = [Option<SocketWaitSource>; 3];
    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::ipc::{Pipe, ReceiveBuffer};
        use super::NETWORK_TEST_SERIAL as SERIAL;
        fn address() -> PacketAddress {
            PacketAddress {
                protocol: 0x0800u16.to_be(),
                interface_index: 2,
                hardware_type: 1,
                packet_type: 0,
                address_length: 6,
                address: [2, 0, 0, 0, 0, 2, 0, 0],
            }
        }
        fn frame() -> [u8; 18] {
            [2, 0, 0, 0, 0, 1, 2, 0, 0, 0, 0, 2, 8, 0, 1, 2, 3, 4]
        }
        #[test]
        fn packet_fput_retires_queued_data_and_membership_despite_observer() {
            let _serial = SERIAL.lock().unwrap();
            crate::namespace::boot();
            initialize_net().unwrap();
            packet::init();
            inet::init();
            inet::configure_up(net_namespace::initial_net_ref(), 2, true).unwrap();
            let endpoint = packet::PacketSocket::new(
                SocketType::Datagram,
                0x0800u16.to_be() as usize,
                Pipe::notification_pair().unwrap(),
                SocketMemory::try_new(initial_net()).unwrap(),
            )
            .unwrap();
            endpoint.bind(address()).unwrap();
            packet::deliver(initial_net().id(), 2, [2, 0, 0, 0, 0, 1], &frame());
            assert!(endpoint.poll_state().readable);
            let pending = packet::take_pending_notification(0).unwrap().1;
            endpoint.close_description();
            assert_eq!(endpoint.poll_state(), SocketPollState::closed());
            assert_eq!(endpoint.bind(address()), Err(SocketError::NotConnected));
            assert_eq!(
                endpoint.send_to(b"closed", Some(address())),
                Err(SocketError::NotConnected)
            );
            let mut output = [0; 16];
            assert_eq!(
                endpoint.receive(&mut ReceiveBuffer::from_slice(&mut output), false),
                Err(SocketError::NotConnected)
            );
            packet::deliver(initial_net().id(), 2, [2, 0, 0, 0, 0, 1], &frame());
            assert!(packet::take_pending_notification(0).is_none());
            let replacement = packet::PacketSocket::new(
                SocketType::Datagram,
                0x0800u16.to_be() as usize,
                Pipe::notification_pair().unwrap(),
                SocketMemory::try_new(initial_net()).unwrap(),
            )
            .unwrap();
            replacement.bind(address()).unwrap();
            pending.close_description();
            drop(endpoint);
            drop(pending);
            packet::deliver(initial_net().id(), 2, [2, 0, 0, 0, 0, 1], &frame());
            assert_eq!(
                replacement
                    .receive(&mut ReceiveBuffer::from_slice(&mut output), false)
                    .unwrap()
                    .0,
                4
            );
            assert_eq!(&output[..4], &[1, 2, 3, 4]);
            replacement.close_description();
        }
        #[test]
        fn packet_payload_oom_preserves_queue_and_read_and_fanout_do_not_allocate() {
            let _serial = SERIAL.lock().unwrap();
            crate::namespace::boot();
            initialize_net().unwrap();
            packet::init();
            inet::init();
            inet::configure_up(net_namespace::initial_net_ref(), 2, true).unwrap();
            let endpoints: Vec<_> = (0..8)
                .map(|_| {
                    let endpoint = packet::PacketSocket::new(
                        SocketType::Datagram,
                        0x0800u16.to_be() as usize,
                        Pipe::notification_pair().unwrap(),
                        SocketMemory::try_new(initial_net()).unwrap(),
                    )
                    .unwrap();
                    endpoint.bind(address()).unwrap();
                    endpoint
                })
                .collect();
            for fault in 0..2 {
                crate::allocation_failure::fail_after(fault);
                packet::deliver(initial_net().id(), 2, [2, 0, 0, 0, 0, 1], &frame());
                assert!(crate::allocation_failure::was_consumed());
                assert!(
                    endpoints
                        .iter()
                        .all(|endpoint| !endpoint.poll_state().readable)
                );
            }
            // 同一 frame 只分配 payload Vec/Arc 两次，八个实际 endpoint queue publication 不分配。
            crate::allocation_failure::fail_after(2);
            packet::deliver(initial_net().id(), 2, [2, 0, 0, 0, 0, 1], &frame());
            assert!(!crate::allocation_failure::was_consumed());
            for endpoint in endpoints {
                crate::allocation_failure::fail_next();
                let mut output = [0; 16];
                assert_eq!(
                    endpoint
                        .receive(&mut ReceiveBuffer::from_slice(&mut output), true)
                        .unwrap()
                        .0,
                    4
                );
                assert_eq!(
                    endpoint
                        .receive(&mut ReceiveBuffer::from_slice(&mut output), false)
                        .unwrap()
                        .0,
                    4
                );
                assert!(!crate::allocation_failure::was_consumed());
                endpoint.close_description();
            }
        }
        include!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tools/kernel-unit/tests/netlink_endpoint.rs.inc"
        ));
        #[test]
        fn netlink_fput_releases_port_and_rejects_observer_rebind_or_receive() {
            let _serial = SERIAL.lock().unwrap();
            crate::namespace::boot();
            initialize_net().unwrap();
            let endpoint = kobject::KobjectSocket::new(
                Pipe::notification_pair().unwrap(),
                SocketMemory::try_new(initial_net()).unwrap(),
            )
            .unwrap();
            let address = NetlinkAddress {
                port_id: 9021,
                groups: 1,
            };
            endpoint.bind(address, 9021).unwrap();
            let observer = endpoint.clone();
            kobject::publish_drm_hotplug();
            assert!(endpoint.poll_state().readable);
            endpoint.close_description();
            assert_eq!(endpoint.poll_state(), SocketPollState::closed());
            assert_eq!(observer.bind(address, 9021), Err(SocketError::NotConnected));
            let replacement = kobject::KobjectSocket::new(
                Pipe::notification_pair().unwrap(),
                SocketMemory::try_new(initial_net()).unwrap(),
            )
            .unwrap();
            replacement.bind(address, 9021).unwrap();
            observer.close_description();
            drop(endpoint);
            let mut output = [0; 256];
            assert!(matches!(
                observer.receive(&mut ReceiveBuffer::from_slice(&mut output), false),
                Err(SocketError::NotConnected)
            ));
            kobject::publish_drm_hotplug();
            assert!(!observer.poll_state().writable);
            assert!(
                replacement
                    .receive(&mut ReceiveBuffer::from_slice(&mut output), false)
                    .unwrap()
                    .count
                    > 0
            );
            drop(observer);
            replacement.close_description();
        }
    }
}

pub(crate) use socket::inet::lifecycle_benchmark;
