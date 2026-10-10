#![feature(allocator_ext)]
#![allow(dead_code)]
//! 完整实际 memory/NET policy；独立 binary 的调优、并发收费及成本诊断，不运行 transport 模型。
extern crate alloc;
#[path = "../src/allocation_failure.rs"]
mod allocation_failure;
#[path = "../../../kernel/src/fallible_tree/mod.rs"]
mod fallible_tree;
#[path = "../src/namespace_fixture.rs"]
mod namespace;
#[path = "../src/socket_option_memory_fixture.rs"]
mod socket;

#[test]
fn net_sysctl_permission_uses_global_root_and_namespace_capability_not_stat_owner() {
    socket::endpoint();
    let root = namespace::root_authority();
    let child = namespace::UserNamespace::child(
        &root,
        true,
        namespace::NamespaceRlimits {
            locked_memory: u64::MAX,
            pending_signals: u64::MAX,
        },
    )
    .unwrap();
    child
        .publish_map(true, b"0 1000 1\n", &root, &root)
        .unwrap();
    let net = socket::NetNamespace::create(child.clone(), 1000).unwrap();
    assert_eq!(net.user_namespace().to_kernel(true, 0), Some(1000));
    let mapped_owner = namespace::NamespaceAuthority {
        namespace: child.clone(),
        effective_uid: 1000,
        effective_gid: 1000,
        effective_capabilities: 0,
    };
    assert!(!net.may_set_limits(&mapped_owner));
    assert!(matches!(
        net.set_optmem_max(41, &mapped_owner),
        Err(namespace::NamespaceError::Permission)
    ));
    let admin = namespace::NamespaceAuthority {
        effective_capabilities: syscall_abi::capability::Capability::NET_ADMIN.mask(),
        ..mapped_owner
    };
    net.set_optmem_max(41, &admin).unwrap();
    let global_root = namespace::NamespaceAuthority {
        effective_capabilities: 0,
        ..root
    };
    net.set_optmem_max(42, &global_root).unwrap();
    assert_eq!(net.optmem_max(), 42);
}
#[test]
fn actual_policy_and_concurrent_reservations_preserve_strict_budget_and_refund() {
    use std::sync::{
        Arc, Barrier,
        atomic::{AtomicUsize, Ordering},
    };
    let first = socket::endpoint();
    let second = socket::endpoint();
    let owner = socket::initial_net();
    let root = namespace::root_authority();
    struct Restore(
        alloc::sync::Arc<socket::NetNamespace>,
        namespace::NamespaceAuthority,
        u32,
    );
    impl Drop for Restore {
        fn drop(&mut self) {
            self.0.set_optmem_max(self.2, &self.1).unwrap();
        }
    }
    let _restore = Restore(owner.clone(), root.clone(), owner.optmem_max() as u32);
    assert!(Arc::ptr_eq(&first.memory.namespace(), &owner));
    assert!(Arc::ptr_eq(&second.memory.namespace(), &owner));
    owner.set_optmem_max(4096, &root).unwrap();
    assert!(first.memory.reserve(4096).is_none());
    let charge = first.memory.reserve(4095).unwrap();
    assert!(first.memory.reserve(1).is_none());
    assert!(second.memory.reserve(4095).is_some());
    // 缩小不能提前释放旧 charge；新 admission 按同一实际 NET scalar 判定。
    owner.set_optmem_max(64, &root).unwrap();
    assert!(first.memory.reserve(1).is_none());
    assert!(second.memory.reserve(63).is_some());
    drop(charge);
    assert!(first.memory.reserve(63).is_some());
    owner.set_optmem_max(0, &root).unwrap();
    assert!(first.memory.reserve(0).is_none());
    owner.set_optmem_max(4096, &root).unwrap();
    let mut denied = root.clone();
    denied.effective_uid = 1000;
    denied.effective_capabilities = 0;
    assert!(matches!(
        owner.set_optmem_max(1, &denied),
        Err(namespace::NamespaceError::Permission)
    ));
    assert!(matches!(
        owner.set_optmem_max(u32::MAX, &root),
        Err(namespace::NamespaceError::Invalid)
    ));
    assert_eq!(owner.optmem_max(), 4096);
    let start = Arc::new(Barrier::new(17));
    let held = Arc::new(Barrier::new(17));
    let release = Arc::new(Barrier::new(17));
    let accepted = Arc::new(AtomicUsize::new(0));
    let mut workers = Vec::new();
    for _ in 0..16 {
        let (memory, start, held, release, accepted) = (
            first.memory.clone(),
            start.clone(),
            held.clone(),
            release.clone(),
            accepted.clone(),
        );
        workers.push(std::thread::spawn(move || {
            start.wait();
            let charge = memory.reserve(256);
            if charge.is_some() {
                accepted.fetch_add(1, Ordering::Relaxed);
            }
            held.wait();
            release.wait();
            drop(charge);
        }));
    }
    start.wait();
    held.wait();
    assert_eq!(accepted.load(Ordering::Relaxed), 15);
    release.wait();
    for worker in workers {
        worker.join().unwrap();
    }
    assert!(first.memory.reserve(4095).is_some());
    let mut durations = Vec::new();
    for _ in 0..5 {
        let start = std::time::Instant::now();
        for _ in 0..200_000 {
            drop(std::hint::black_box(first.memory.reserve(64).unwrap()));
        }
        durations.push(start.elapsed().as_nanos());
    }
    durations.sort_unstable();
    eprintln!(
        "actual optmem reserve/refund median: {} ns per operation",
        durations[2] / 200_000
    );
}
