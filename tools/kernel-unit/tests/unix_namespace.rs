#![feature(allocator_ext)]
#![allow(dead_code)]
//! 直接装配实际 AF_UNIX registry；endpoint seam 仅提供 address owner。
extern crate alloc;
#[path = "../src/allocation_failure.rs"]
mod allocation_failure;
#[path = "../../../kernel/src/fallible_tree/mod.rs"]
mod fallible_tree;
#[path = "../../../kernel/src/socket/unix/namespace.rs"]
mod namespace;
use alloc::sync::Arc;
use namespace::{BindingRequest, NamespaceKey, UnixAddress, UnixPathIdentity};
use spin::Mutex;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SocketError {
    Invalid,
    AddressInUse,
    NoMemory,
    NoSpace,
    ConnectionRefused,
}
#[derive(Clone)]
struct BoundAddress {
    visible: UnixAddress,
    binding: Option<NamespaceKey>,
    path_owner: Option<Arc<dyn Send + Sync>>,
}
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum SocketType {
    Stream,
    Datagram,
    SeqPacket,
}
enum SocketState {
    Initial,
    Closed,
}
struct UnixSocket {
    net_id: u64,
    address: Mutex<Option<BoundAddress>>,
    state: Mutex<SocketState>,
    socket_type: SocketType,
}
impl UnixSocket {
    fn network_namespace_id(&self) -> u64 {
        self.net_id
    }
    fn is_closed(&self) -> bool {
        matches!(*self.state.lock(), SocketState::Closed)
    }
}
fn endpoint() -> Arc<UnixSocket> {
    Arc::new(UnixSocket {
        net_id: 6,
        address: Mutex::new(None),
        state: Mutex::new(SocketState::Initial),
        socket_type: SocketType::Stream,
    })
}
fn address(seed: u8) -> UnixAddress {
    UnixAddress::new(&[0, seed, 0, 255]).unwrap()
}
#[test]
fn preparation_failure_preserves_endpoint_and_registry_then_same_bind_retries() {
    let socket = endpoint();
    let visible = address(1);
    let key = NamespaceKey::Abstract(6, visible, SocketType::Stream);
    allocation_failure::fail_next();
    let result = namespace::register(
        &socket,
        BindingRequest::Named {
            path_owner: None,
            key,
            address: visible,
        },
    );
    let consumed = allocation_failure::was_consumed();
    assert_eq!(result, Err(SocketError::NoMemory));
    assert!(consumed);
    assert!(socket.address.lock().is_none());
    assert!(matches!(
        namespace::lookup(&key),
        Err(SocketError::ConnectionRefused)
    ));
    namespace::register(
        &socket,
        BindingRequest::Named {
            path_owner: None,
            key,
            address: visible,
        },
    )
    .unwrap();
    assert!(Arc::ptr_eq(&namespace::lookup(&key).unwrap(), &socket));
    drop(socket);
    namespace::remove_closed(&key);
}
#[test]
fn stale_binding_replacement_survives_delayed_close_of_previous_endpoint() {
    let visible = address(2);
    let key = NamespaceKey::Abstract(6, visible, SocketType::Stream);
    let old = endpoint();
    namespace::register(
        &old,
        BindingRequest::Named {
            path_owner: None,
            key,
            address: visible,
        },
    )
    .unwrap();
    drop(old);
    let new = endpoint();
    namespace::register(
        &new,
        BindingRequest::Named {
            path_owner: None,
            key,
            address: visible,
        },
    )
    .unwrap();
    namespace::remove_closed(&key);
    assert!(Arc::ptr_eq(&namespace::lookup(&key).unwrap(), &new));
    let collision = endpoint();
    assert_eq!(
        namespace::register(
            &collision,
            BindingRequest::Named {
                path_owner: None,
                key,
                address: visible
            }
        ),
        Err(SocketError::AddressInUse)
    );
    assert!(collision.address.lock().is_none());
    drop(new);
    namespace::remove_closed(&key);
    assert!(matches!(
        namespace::lookup(&key),
        Err(SocketError::ConnectionRefused)
    ));
}
#[test]
fn concurrent_bind_commits_exactly_one_address_on_the_same_endpoint() {
    let socket = endpoint();
    let first = address(3);
    let second = address(4);
    let results = std::thread::scope(|scope| {
        let a = scope.spawn(|| {
            namespace::register(
                &socket,
                BindingRequest::Named {
                    path_owner: None,
                    key: NamespaceKey::Abstract(6, first, SocketType::Stream),
                    address: first,
                },
            )
        });
        let b = scope.spawn(|| {
            namespace::register(
                &socket,
                BindingRequest::Named {
                    path_owner: None,
                    key: NamespaceKey::Abstract(6, second, SocketType::Stream),
                    address: second,
                },
            )
        });
        [a.join().unwrap(), b.join().unwrap()]
    });
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    let bound = socket.address.lock().as_ref().unwrap().clone();
    for visible in [first, second] {
        let lookup = namespace::lookup(&NamespaceKey::Abstract(6, visible, SocketType::Stream));
        if visible == bound.visible {
            assert!(Arc::ptr_eq(&lookup.unwrap(), &socket));
        } else {
            assert!(matches!(lookup, Err(SocketError::ConnectionRefused)));
        }
    }
    drop(socket);
    namespace::remove_closed(&bound.binding.unwrap());
}
#[test]
fn pathname_registry_uses_inode_identity_and_abstract_names_preserve_embedded_nuls() {
    let first = endpoint();
    let second = endpoint();
    let path = UnixAddress::new(b"/tmp/socket").unwrap();
    let a = NamespaceKey::Path(UnixPathIdentity {
        filesystem: 9,
        inode: 1,
    });
    let b = NamespaceKey::Path(UnixPathIdentity {
        filesystem: 9,
        inode: 2,
    });
    namespace::register(
        &first,
        BindingRequest::Named {
            path_owner: None,
            key: a,
            address: path,
        },
    )
    .unwrap();
    namespace::register(
        &second,
        BindingRequest::Named {
            path_owner: None,
            key: b,
            address: path,
        },
    )
    .unwrap();
    assert!(Arc::ptr_eq(&namespace::lookup(&a).unwrap(), &first));
    assert!(Arc::ptr_eq(&namespace::lookup(&b).unwrap(), &second));
    assert_ne!(
        UnixAddress::new(&[0, 8]).unwrap(),
        UnixAddress::new(&[0, 8, 0]).unwrap()
    );
    drop(first);
    drop(second);
    namespace::remove_closed(&a);
    namespace::remove_closed(&b);
}

#[test]
fn closed_endpoint_observer_does_not_pin_binding_or_hide_replacement() {
    let visible = address(7);
    let key = NamespaceKey::Abstract(6, visible, SocketType::Stream);
    let old = endpoint();
    namespace::register(
        &old,
        BindingRequest::Named {
            path_owner: None,
            key,
            address: visible,
        },
    )
    .unwrap();
    let observer = old.clone();
    *old.state.lock() = SocketState::Closed;
    assert!(matches!(
        namespace::lookup(&key),
        Err(SocketError::ConnectionRefused)
    ));
    let new = endpoint();
    namespace::register(
        &new,
        BindingRequest::Named {
            path_owner: None,
            key,
            address: visible,
        },
    )
    .unwrap();
    namespace::remove_closed(&key);
    assert!(Arc::ptr_eq(&namespace::lookup(&key).unwrap(), &new));
    drop(old);
    drop(observer);
    assert!(Arc::ptr_eq(&namespace::lookup(&key).unwrap(), &new));
    drop(new);
    namespace::remove_closed(&key);
}

#[test]
fn autobind_collision_reuses_prepared_node_and_repeated_bind_does_not_allocate() {
    let first = endpoint();
    namespace::register(&first, BindingRequest::Automatic { seed: 0x70a00 }).unwrap();
    let first_bound = first.address.lock().as_ref().unwrap().clone();
    assert_eq!(first_bound.visible.bytes(), b"\x0070a01");
    let second = endpoint();
    allocation_failure::fail_after(1);
    let result = namespace::register(&second, BindingRequest::Automatic { seed: 0x70a00 });
    let extra_allocation = allocation_failure::was_consumed();
    assert_eq!(result, Ok(()));
    assert!(
        !extra_allocation,
        "collision retry must reuse the prepared actual AVL node"
    );
    let second_bound = second.address.lock().as_ref().unwrap().clone();
    assert_eq!(second_bound.visible.bytes(), b"\x0070a02");
    allocation_failure::fail_next();
    let result = namespace::register(&second, BindingRequest::Automatic { seed: 0 });
    let allocated = allocation_failure::was_consumed();
    assert_eq!(result, Ok(()));
    assert!(!allocated);
    assert_eq!(
        second.address.lock().as_ref().unwrap().clone().visible,
        second_bound.visible
    );
    drop(first);
    drop(second);
    namespace::remove_closed(&first_bound.binding.unwrap());
    namespace::remove_closed(&second_bound.binding.unwrap());
}

#[test]
fn autobind_oom_is_retryable_and_closed_observer_cannot_publish_a_new_binding() {
    let socket = endpoint();
    allocation_failure::fail_next();
    let result = namespace::register(&socket, BindingRequest::Automatic { seed: 0x70b00 });
    let consumed = allocation_failure::was_consumed();
    assert_eq!(result, Err(SocketError::NoMemory));
    assert!(consumed);
    assert!(socket.address.lock().is_none());
    namespace::register(&socket, BindingRequest::Automatic { seed: 0x70b00 }).unwrap();
    let bound = socket.address.lock().take().unwrap();
    *socket.state.lock() = SocketState::Closed;
    namespace::remove_closed(&bound.binding.unwrap());
    assert_eq!(
        namespace::register(&socket, BindingRequest::Automatic { seed: 1 }),
        Err(SocketError::Invalid)
    );
    assert_eq!(
        namespace::register(
            &socket,
            BindingRequest::Named {
                path_owner: None,
                key: bound.binding.unwrap(),
                address: bound.visible
            }
        ),
        Err(SocketError::Invalid)
    );
    assert!(socket.address.lock().is_none());
    assert!(matches!(
        namespace::lookup(&bound.binding.unwrap()),
        Err(SocketError::ConnectionRefused)
    ));
}

#[test]
fn abstract_address_namespace_is_partitioned_by_actual_socket_type() {
    let stream = endpoint();
    let datagram = Arc::new(UnixSocket {
        net_id: 6,
        address: Mutex::new(None),
        state: Mutex::new(SocketState::Initial),
        socket_type: SocketType::Datagram,
    });
    for socket in [&stream, &datagram] {
        namespace::register(socket, BindingRequest::Automatic { seed: 0x70c00 }).unwrap();
    }
    let a = stream.address.lock().as_ref().unwrap().clone();
    let b = datagram.address.lock().as_ref().unwrap().clone();
    assert_eq!(a.visible, b.visible);
    assert_ne!(a.binding, b.binding);
    assert!(Arc::ptr_eq(
        &namespace::lookup(&a.binding.unwrap()).unwrap(),
        &stream
    ));
    assert!(Arc::ptr_eq(
        &namespace::lookup(&b.binding.unwrap()).unwrap(),
        &datagram
    ));
    drop(stream);
    drop(datagram);
    namespace::remove_closed(&a.binding.unwrap());
    namespace::remove_closed(&b.binding.unwrap());
}
#[test]
fn automatic_names_are_reusable_in_distinct_network_domains_without_a_second_registry() {
    let first = endpoint();
    let second = Arc::new(UnixSocket {
        net_id: 991,
        address: Mutex::new(None),
        state: Mutex::new(SocketState::Initial),
        socket_type: SocketType::Stream,
    });
    for socket in [&first, &second] {
        namespace::register(socket, BindingRequest::Automatic { seed: 0x70d00 }).unwrap();
    }
    let a = first.address.lock().as_ref().unwrap().clone();
    let b = second.address.lock().as_ref().unwrap().clone();
    assert_eq!(a.visible, b.visible);
    assert_ne!(a.binding, b.binding);
    assert!(Arc::ptr_eq(
        &namespace::lookup(&a.binding.unwrap()).unwrap(),
        &first
    ));
    assert!(Arc::ptr_eq(
        &namespace::lookup(&b.binding.unwrap()).unwrap(),
        &second
    ));
    drop(first);
    drop(second);
    namespace::remove_closed(&a.binding.unwrap());
    namespace::remove_closed(&b.binding.unwrap());
}
