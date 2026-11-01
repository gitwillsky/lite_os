#![feature(allocator_ext)]
#![allow(dead_code)]
//! Actual Linux NET policy and kobject socket owners; remote protocol construction is a boundary.
extern crate alloc;
extern crate network_protocol as network_wire;
extern crate self as network_protocol;
pub use network_wire::MAX_IPV4_UDP_BYTES;
/// Opaque remote endpoint input: this fixture never performs network IPC.
pub struct RemoteNamespace;
#[path = "../src/allocation_failure.rs"]
mod allocation_failure;
#[path = "../src/pipe_fixture.rs"]
mod ipc;
#[path = "../src/namespace_fixture.rs"]
mod namespace;
#[path = "../src/sync.rs"]
mod sync;
mod file {
    pub(crate) use crate::sync::next_readiness_generation;
}
#[path = "../../../system/linuxd/src/id.rs"]
mod id;
#[path = "../../../system/linuxd/src/net"]
mod net {
    use alloc::sync::Arc;
    mod net_namespace;
    use net_namespace::{NetNamespace, initial_net, initialize_net};
    mod netd {
        pub(super) fn create_namespace() -> Result<crate::RemoteNamespace, super::SocketError> {
            Ok(crate::RemoteNamespace)
        }
    }
    fn create_net_namespace(
        user: Arc<crate::namespace::UserNamespace>,
        uid: u32,
    ) -> Result<Arc<NetNamespace>, crate::namespace::NamespaceError> {
        NetNamespace::create(user, uid)
    }
    mod core_buffer_limit;
    use core_buffer_limit::CoreBufferLimit;
    mod control_memory;
    mod core_buffers;
    use control_memory::SocketMemory;
    mod unix {
        pub(super) fn minimum_buffer(_: super::SocketIoDirection) -> usize {
            512
        }
    }
    #[derive(Clone, Copy)]
    pub(crate) enum SocketIoDirection {
        Send,
        Receive,
    }
    mod kobject;
    mod message_limits;
    #[derive(Clone, Copy)]
    pub(crate) enum SocketDomain {
        Unix,
        Inet,
        Packet,
        Netlink,
    }
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

    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    use crate::ipc::{Pipe, ReceiveBuffer};
    // 实际production KobjectSocket/NET/端口/队列；只从标准endpoint API观察，无第二份状态模型。
    fn scoped_netlink(net: Arc<NetNamespace>) -> Arc<kobject::KobjectSocket> {
        kobject::KobjectSocket::new(
            Pipe::notification_pair().unwrap(),
            SocketMemory::try_new(net).unwrap(),
        )
        .unwrap()
    }
    fn netlink_sequence(bytes: &[u8]) -> u64 {
        let record = bytes
            .split(|byte| *byte == 0)
            .find(|record| record.starts_with(b"SEQNUM="))
            .unwrap();
        core::str::from_utf8(&record[7..]).unwrap().parse().unwrap()
    }
    #[test]
    fn netlink_actual_scoped_ports_autobind_rebind_and_oom() {
        let _serial = SERIAL.lock().unwrap();
        crate::namespace::boot();
        initialize_net().unwrap();
        let net = create_net_namespace(crate::namespace::initial_user(), 89230).unwrap();
        let first = scoped_netlink(initial_net());
        let second = scoped_netlink(initial_net());
        let isolated = scoped_netlink(net);
        let auto = NetlinkAddress {
            port_id: 0,
            groups: u32::MAX,
        };
        first.bind(auto, 9022).unwrap();
        assert_eq!(
            first.address(),
            NetlinkAddress {
                port_id: 9022,
                groups: 1
            }
        );
        second.bind(auto, 9022).unwrap();
        assert!((second.address().port_id as i32) <= -4097);
        assert_ne!(second.address().port_id, first.address().port_id);
        isolated
            .bind(
                NetlinkAddress {
                    port_id: 9022,
                    groups: 0,
                },
                7,
            )
            .unwrap();
        let third = scoped_netlink(initial_net());
        assert_eq!(
            third.bind(
                NetlinkAddress {
                    port_id: 9022,
                    groups: 0
                },
                9022
            ),
            Err(SocketError::AddressInUse)
        );
        crate::allocation_failure::fail_next();
        first
            .bind(
                NetlinkAddress {
                    port_id: 9022,
                    groups: 2,
                },
                9022,
            )
            .unwrap();
        assert!(!crate::allocation_failure::was_consumed());
        assert_eq!(first.address().groups, 0);
        assert_eq!(first.bind(auto, 9022), Err(SocketError::Invalid));
        crate::allocation_failure::fail_next();
        assert_eq!(
            third.bind(
                NetlinkAddress {
                    port_id: 9023,
                    groups: 1
                },
                9022
            ),
            Err(SocketError::NoMemory)
        );
        assert!(crate::allocation_failure::was_consumed());
        assert_eq!(third.address().port_id, 0);
        third
            .bind(
                NetlinkAddress {
                    port_id: 9023,
                    groups: 1,
                },
                9022,
            )
            .unwrap();
        for socket in [first, second, isolated, third] {
            socket.close_description();
        }
    }
    #[test]
    fn untagged_kernel_uevent_reaches_all_initial_user_nets_and_no_child_user_net() {
        let _serial = SERIAL.lock().unwrap();
        crate::namespace::boot();
        initialize_net().unwrap();
        let root = crate::namespace::root_authority();
        let child = crate::namespace::UserNamespace::child(
            &root,
            true,
            crate::namespace::NamespaceRlimits {
                locked_memory: u64::MAX,
                pending_signals: u64::MAX,
            },
        )
        .unwrap();
        let a = scoped_netlink(initial_net());
        let b = scoped_netlink(create_net_namespace(root.namespace, 89231).unwrap());
        let c = scoped_netlink(create_net_namespace(child, 89232).unwrap());
        for socket in [&a, &b, &c] {
            socket
                .bind(
                    NetlinkAddress {
                        port_id: 9024,
                        groups: 1,
                    },
                    9024,
                )
                .unwrap();
        }
        kobject::publish_drm_hotplug();
        let mut first = [0; 256];
        let mut second = [0; 256];
        let one = a
            .receive(&mut ReceiveBuffer::from_slice(&mut first), false)
            .unwrap();
        let two = b
            .receive(&mut ReceiveBuffer::from_slice(&mut second), false)
            .unwrap();
        assert_eq!(&first[..one.count], &second[..two.count]);
        assert_eq!(
            one.source,
            Some(SocketAddress::Netlink(NetlinkAddress {
                port_id: 0,
                groups: 1
            }))
        );
        assert!(!c.poll_state().readable);
        assert!(matches!(
            c.receive(&mut ReceiveBuffer::from_slice(&mut first), false),
            Err(SocketError::Again)
        ));
        for socket in [a, b, c] {
            socket.close_description();
        }
    }
    #[test]
    fn netlink_actual_fifo_exceeds_old_fixed_capacity_and_overrun_never_overwrites() {
        let _serial = SERIAL.lock().unwrap();
        crate::namespace::boot();
        initialize_net().unwrap();
        let endpoint = scoped_netlink(initial_net());
        endpoint
            .bind(
                NetlinkAddress {
                    port_id: 9025,
                    groups: 1,
                },
                9025,
            )
            .unwrap();
        for _ in 0..64 {
            kobject::publish_drm_hotplug();
        }
        let mut bytes = [0; 256];
        let mut previous = 0;
        for _ in 0..64 {
            let peek = endpoint
                .receive(&mut ReceiveBuffer::from_slice(&mut bytes), true)
                .unwrap();
            let sequence = netlink_sequence(&bytes[..peek.count]);
            assert!(sequence > previous);
            let received = endpoint
                .receive(&mut ReceiveBuffer::from_slice(&mut bytes), false)
                .unwrap();
            assert_eq!(netlink_sequence(&bytes[..received.count]), sequence);
            previous = sequence;
        }
        assert!(!endpoint.poll_state().readable);
        endpoint
            .set_buffer_limit(SocketIoDirection::Receive, 0, false)
            .unwrap();
        assert!(endpoint.buffer_limit(SocketIoDirection::Receive).unwrap() >= 2048);
        let mut events = 0;
        while !endpoint.poll_state().error && events < 64 {
            kobject::publish_drm_hotplug();
            events += 1;
        }
        assert!(events > 1 && events < 64);
        assert_eq!(endpoint.take_error(), Some(SocketError::NoBuffer));
        assert_eq!(endpoint.take_error(), None);
        kobject::publish_drm_hotplug();
        assert!(
            !endpoint.poll_state().error,
            "one overrun reports once until actual FIFO drains"
        );
        let first = endpoint
            .receive(&mut ReceiveBuffer::from_slice(&mut bytes), true)
            .unwrap();
        let earliest = netlink_sequence(&bytes[..first.count]);
        let mut drained = 0;
        previous = 0;
        while endpoint.poll_state().readable {
            let received = endpoint
                .receive(&mut ReceiveBuffer::from_slice(&mut bytes), false)
                .unwrap();
            let sequence = netlink_sequence(&bytes[..received.count]);
            if drained == 0 {
                assert_eq!(sequence, earliest);
            }
            assert!(sequence > previous);
            previous = sequence;
            drained += 1;
        }
        assert_eq!(drained, events - 1);
        kobject::publish_drm_hotplug();
        assert!(endpoint.poll_state().readable && !endpoint.poll_state().error);
        endpoint
            .receive(&mut ReceiveBuffer::from_slice(&mut bytes), false)
            .unwrap();
        crate::allocation_failure::fail_next();
        kobject::publish_drm_hotplug();
        assert!(crate::allocation_failure::was_consumed());
        assert!(matches!(
            endpoint.receive(&mut ReceiveBuffer::from_slice(&mut bytes), false),
            Err(SocketError::NoBuffer)
        ));
        assert!(matches!(
            endpoint.receive(&mut ReceiveBuffer::from_slice(&mut bytes), false),
            Err(SocketError::Again)
        ));
        kobject::publish_drm_hotplug();
        assert!(
            endpoint
                .receive(&mut ReceiveBuffer::from_slice(&mut bytes), false)
                .is_ok()
        );
        endpoint.close_description();
    }

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
