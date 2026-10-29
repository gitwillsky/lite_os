#![feature(allocator_ext)]
#![allow(dead_code)]
//! 原样载入NET域/ucounts/有界目录；不执行协议模型，transport由真实wire binary覆盖。
extern crate alloc;
#[path = "../src/allocation_failure.rs"]
mod allocation_failure;
#[path = "../../../kernel/src/fallible_tree/mod.rs"]
mod fallible_tree;
#[path = "../src/namespace_fixture.rs"]
mod namespace;
#[path = "../../../kernel/src/net"]
mod net {
    mod inet {
        // 未安装protocol的类型边界：本binary只覆盖实际目录和namespace lifetime。
        pub(crate) struct NetworkStackOwner;
    }
    mod net_namespace;
    pub(crate) use net_namespace::{NetNamespace, initial_net, initialize_net};
    pub(crate) fn batch(timer: bool) -> (alloc::vec::Vec<alloc::sync::Arc<NetNamespace>>, bool) {
        use net_namespace::{NetworkNamespaceScan, take_network_namespaces};
        let batch = take_network_namespaces(if timer {
            NetworkNamespaceScan::Timer
        } else {
            NetworkNamespaceScan::Deferred
        })
        .unwrap();
        (
            batch.namespaces.into_iter().flatten().collect(),
            batch.backlog,
        )
    }
}

#[test]
fn actual_directory_is_bounded_fair_nonowning_and_allocation_failure_refunds() {
    namespace::boot();
    net::initialize_net().unwrap();
    let user = namespace::initial_user();
    let mut domains = alloc::vec::Vec::new();
    for _ in 0..24 {
        domains.push(net::NetNamespace::create(user.clone(), 89120).unwrap());
    }
    let (first, backlog) = net::batch(false);
    assert_eq!(first.len(), 8);
    assert!(backlog);
    let (timer, timer_backlog) = net::batch(true);
    assert!(timer_backlog);
    assert_eq!(
        first
            .iter()
            .map(|net| net.id())
            .collect::<alloc::vec::Vec<_>>(),
        timer
            .iter()
            .map(|net| net.id())
            .collect::<alloc::vec::Vec<_>>()
    );
    drop(timer);
    let held_id = first[1].id();
    let held = alloc::sync::Arc::downgrade(&first[1]);
    domains.retain(|net| net.id() != held_id);
    assert!(held.upgrade().is_some());
    drop(first);
    assert!(held.upgrade().is_none(), "目录Weak不能保活实际NET");
    let mut observed = alloc::collections::BTreeSet::new();
    loop {
        let (batch, more) = net::batch(false);
        assert!(batch.len() <= 8);
        for net in batch {
            assert!(observed.insert(net.id()), "一次cursor遍历不能重复同一域");
        }
        if !more {
            break;
        }
    }
    assert_eq!(observed.len(), 17);
    let (restarted, more) = net::batch(false);
    assert!(more);
    assert_eq!(restarted[0].id(), net::initial_net().id());
    drop(restarted);
    drop(domains);
    // 旧cursor域全退休后回到首项；不用重建目录或保留强引用。
    let (empty_tail, more) = net::batch(false);
    assert!(!more);
    drop(empty_tail);
    let (initial, more) = net::batch(false);
    assert_eq!(initial.len(), 1);
    assert_eq!(initial[0].id(), net::initial_net().id());
    assert!(!more);
    drop(initial);
    let root = namespace::root_authority();
    let resource = namespace::UserResource::Net;
    let previous = user.resource_limit(resource);
    user.set_resource_limit(resource, 1, &root).unwrap();
    for boundary in 0..8 {
        allocation_failure::fail_after(boundary);
        let result = net::NetNamespace::create(user.clone(), 89121);
        if allocation_failure::was_consumed() {
            assert!(result.is_err());
        }
        drop(result);
        let net = net::NetNamespace::create(user.clone(), 89121).unwrap();
        assert!(matches!(
            net::NetNamespace::create(user.clone(), 89121),
            Err(namespace::NamespaceError::QuotaLimit)
        ));
        drop(net);
    }
    user.set_resource_limit(resource, previous, &root).unwrap();
    let domains: alloc::vec::Vec<_> = (0..16)
        .map(|_| net::NetNamespace::create(user.clone(), 89122).unwrap())
        .collect();
    // 新增global目录/lock只有deferred观察；实际capture/IO没有追加查找。
    let began = std::time::Instant::now();
    for _ in 0..100_000 {
        std::hint::black_box(net::batch(false));
    }
    eprintln!(
        "NET actual bounded directory diagnostic: {:?} / 100000 batches",
        began.elapsed()
    );
    drop(domains);
}
