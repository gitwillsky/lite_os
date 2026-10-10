#![feature(allocator_ext)]
#![allow(dead_code)]
//! 实际NET policy/ucounts和完整UNIX endpoint；接口与Task publication仍在其专用owner继续装配。
extern crate alloc;
#[path = "../src/allocation_failure.rs"]
mod allocation_failure;
#[path = "../../../kernel/src/fallible_tree/mod.rs"]
mod fallible_tree;
#[path = "../src/unix_rights_fixture/file.rs"]
mod file;
#[path = "../../../kernel/src/id.rs"]
mod id;
#[path = "../src/pipe_fixture.rs"]
mod ipc;
#[path = "../src/namespace_fixture.rs"]
mod namespace;
#[path = "../src/unix_endpoint_fixture.rs"]
mod socket;
#[path = "../src/sync.rs"]
mod sync;
use alloc::sync::Arc;
use socket::{NetNamespace, SocketError, SocketType, UnixAddress, UnixSocket};
fn endpoint(net: &Arc<NetNamespace>) -> Arc<UnixSocket> {
    UnixSocket::new(
        SocketType::Stream,
        ipc::Pipe::notification_pair().unwrap(),
        id::next_runtime_object_id(),
        net.clone(),
    )
    .unwrap()
}
#[test]
fn actual_network_scope_partitions_abstract_names_and_preserves_global_inode_names() {
    let initial = socket::boot_net();
    let root = namespace::root_authority();
    let resource = namespace::UserResource::Net;
    struct Restore(namespace::NamespaceAuthority, u32);
    impl Drop for Restore {
        fn drop(&mut self) {
            self.0
                .namespace
                .set_resource_limit(namespace::UserResource::Net, self.1, &self.0)
                .unwrap();
        }
    }
    let _restore = Restore(root.clone(), root.namespace.resource_limit(resource));
    root.namespace
        .set_resource_limit(resource, 2, &root)
        .unwrap();
    let first = NetNamespace::create(root.namespace.clone(), 88500).unwrap();
    let second = NetNamespace::create(root.namespace.clone(), 88500).unwrap();
    assert!(!first.is_initial());
    assert!(!second.is_initial());
    assert_ne!(first.id(), second.id());
    assert_eq!(first.optmem_max(), 128 * 1024);
    first.set_optmem_max(41, &root).unwrap();
    assert_eq!(second.optmem_max(), 128 * 1024);
    assert_eq!(initial.optmem_max(), 128 * 1024);
    assert!(matches!(
        NetNamespace::create(root.namespace.clone(), 88500),
        Err(namespace::NamespaceError::QuotaLimit)
    ));
    let a = endpoint(&first);
    let b = endpoint(&second);
    let name = UnixAddress::new(b"\0same-abstract-name").unwrap();
    a.bind(name).unwrap();
    b.bind(name).unwrap();
    assert!(Arc::ptr_eq(
        &UnixSocket::lookup(&name, SocketType::Stream, &first).unwrap(),
        &a
    ));
    assert!(Arc::ptr_eq(
        &UnixSocket::lookup(&name, SocketType::Stream, &second).unwrap(),
        &b
    ));
    assert!(matches!(
        UnixSocket::lookup(&name, SocketType::Stream, &initial),
        Err(SocketError::ConnectionRefused)
    ));
    let path_a = endpoint(&first);
    let path_b = endpoint(&second);
    let pathname = UnixAddress::new(b"/actual-inode-scope").unwrap();
    let inode = socket::UnixPathIdentity {
        filesystem: 88301,
        inode: 197,
    };
    path_a.bind_path(pathname, inode, Arc::new(())).unwrap();
    assert!(matches!(
        path_b.bind_path(pathname, inode, Arc::new(())),
        Err(SocketError::AddressInUse)
    ));
    assert!(Arc::ptr_eq(
        &UnixSocket::lookup_path(inode).unwrap(),
        &path_a
    ));
    drop(first);
    assert!(matches!(
        NetNamespace::create(root.namespace.clone(), 88500),
        Err(namespace::NamespaceError::QuotaLimit)
    ));
    socket::close_description(&a);
    socket::close_description(&path_a);
    drop(a);
    drop(path_a);
    let third = NetNamespace::create(root.namespace.clone(), 88500).unwrap();
    assert!(matches!(
        UnixSocket::lookup(&name, SocketType::Stream, &third),
        Err(SocketError::ConnectionRefused)
    ));
    assert!(Arc::ptr_eq(
        &UnixSocket::lookup(&name, SocketType::Stream, &second).unwrap(),
        &b
    ));
    socket::close_description(&b);
    socket::close_description(&path_b);
    drop(b);
    drop(path_b);
    drop(second);
    drop(third);
    // 真实ucounts/Arc的每个fallible allocation失败后，完整新建仍可达到同一额度。
    root.namespace
        .set_resource_limit(resource, 1, &root)
        .unwrap();
    for boundary in 0..4 {
        allocation_failure::fail_after(boundary);
        let preparation = NetNamespace::create(root.namespace.clone(), 88501);
        let consumed = allocation_failure::was_consumed();
        if consumed {
            assert!(preparation.is_err());
        }
        drop(preparation);
        let restored = NetNamespace::create(root.namespace.clone(), 88501).unwrap();
        assert!(matches!(
            NetNamespace::create(root.namespace.clone(), 88501),
            Err(namespace::NamespaceError::QuotaLimit)
        ));
        drop(restored);
    }
}
