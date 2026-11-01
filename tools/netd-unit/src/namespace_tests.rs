#[test]
fn actual_directory_is_bounded_fair_and_nonowning() {
    let _serial = SERIAL.lock().unwrap();
    initialize();
    while domain_batch(false).1 {}
    while domain_batch(true).1 {}
    let mut domains = alloc::vec::Vec::new();
    for _ in 0..24 {
        domains.push(crate::namespace::Namespace::create().unwrap());
    }
    let (first, backlog) = domain_batch(false);
    assert_eq!(first.len(), 8);
    assert!(backlog);
    let (timer, timer_backlog) = domain_batch(true);
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
        let (batch, more) = domain_batch(false);
        assert!(batch.len() <= 8);
        for net in batch {
            assert!(observed.insert(net.id()), "一次cursor遍历不能重复同一域");
        }
        if !more {
            break;
        }
    }
    assert_eq!(observed.len(), 17);
    let (restarted, more) = domain_batch(false);
    assert!(more);
    assert_eq!(restarted[0].id(), namespace().id());
    drop(restarted);
    drop(domains);
    // 旧cursor域全退休后回到首项；不用重建目录或保留强引用。
    let (empty_tail, more) = domain_batch(false);
    assert!(!more);
    drop(empty_tail);
    let (initial, more) = domain_batch(false);
    assert_eq!(initial.len(), 1);
    assert_eq!(initial[0].id(), namespace().id());
    assert!(!more);
    drop(initial);

}
fn domain_batch(timer: bool) -> (Vec<Arc<crate::namespace::Namespace>>, bool) {
    use crate::namespace::{take_network_namespaces, NetworkNamespaceScan};
    let batch = take_network_namespaces(if timer { NetworkNamespaceScan::Timer } else { NetworkNamespaceScan::Deferred }).unwrap();
    (batch.namespaces.into_iter().flatten().collect(), batch.backlog)
}
