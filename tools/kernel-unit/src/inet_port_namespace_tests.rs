use core::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use crate::inet_port_namespace::{
    EPHEMERAL_END, EPHEMERAL_START, PortBinding, PortError, PortNamespace,
};

const LOCAL_A: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 15);
const LOCAL_B: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 16);

#[test]
fn stopped_listener_releases_claim_and_terminal_transport_releases_binding() {
    let mut ports = PortNamespace::new();
    let bound = ports
        .acquire(
            8_090,
            PortBinding::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), false).unwrap(),
            false,
        )
        .unwrap();
    let listening = ports.claim_listener(bound).unwrap();
    let bound = ports.stop_listener(listening);
    let listening = ports.claim_listener(bound).unwrap();
    assert_eq!(
        ports.acquire(
            8_090,
            PortBinding::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), false).unwrap(),
            false
        ),
        Err(PortError::AddressInUse)
    );
    ports.release(listening);
    let rebound = ports
        .acquire(
            8_090,
            PortBinding::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), false).unwrap(),
            false,
        )
        .unwrap();
    ports.release(rebound);
}

#[test]
fn exact_addresses_share_a_port_but_same_address_conflicts() {
    let mut ports = PortNamespace::new();
    let first = ports
        .acquire(
            8_080,
            PortBinding::new(IpAddr::V4(LOCAL_A), false).unwrap(),
            false,
        )
        .unwrap();
    let second = ports
        .acquire(
            8_080,
            PortBinding::new(IpAddr::V4(LOCAL_B), false).unwrap(),
            false,
        )
        .unwrap();

    assert_eq!(
        ports.acquire(
            8_080,
            PortBinding::new(IpAddr::V4(LOCAL_A), false).unwrap(),
            false
        ),
        Err(PortError::AddressInUse)
    );
    ports.release(first);
    ports.release(second);
}

#[test]
fn wildcard_and_exact_memberships_obey_reuse_policy() {
    let mut ports = PortNamespace::new();
    let wildcard = ports
        .acquire(
            8_081,
            PortBinding::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), false).unwrap(),
            true,
        )
        .unwrap();
    let exact = ports
        .acquire(
            8_081,
            PortBinding::new(IpAddr::V4(LOCAL_A), false).unwrap(),
            true,
        )
        .unwrap();

    assert_eq!(
        ports.acquire(
            8_081,
            PortBinding::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), false).unwrap(),
            false
        ),
        Err(PortError::AddressInUse)
    );
    assert_eq!(
        ports.acquire(
            8_081,
            PortBinding::new(IpAddr::V4(LOCAL_A), false).unwrap(),
            false
        ),
        Err(PortError::AddressInUse)
    );
    ports.release(wildcard);
    ports.release(exact);

    let exclusive = ports
        .acquire(
            8_081,
            PortBinding::new(IpAddr::V4(LOCAL_A), false).unwrap(),
            false,
        )
        .unwrap();
    assert_eq!(
        ports.acquire(
            8_081,
            PortBinding::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), false).unwrap(),
            true
        ),
        Err(PortError::AddressInUse)
    );
    ports.release(exclusive);
}

#[test]
fn specific_port_zero_lease_keeps_its_exact_address() {
    let mut ports = PortNamespace::new();
    let specific = ports
        .acquire_ephemeral(PortBinding::new(IpAddr::V4(LOCAL_A), false).unwrap(), false)
        .unwrap();
    let port = specific.port();
    let other_address = ports
        .acquire(
            port,
            PortBinding::new(IpAddr::V4(LOCAL_B), false).unwrap(),
            false,
        )
        .unwrap();

    assert_eq!(
        ports.acquire(
            port,
            PortBinding::new(IpAddr::V4(LOCAL_A), false).unwrap(),
            false
        ),
        Err(PortError::AddressInUse)
    );
    ports.release(specific);
    let rebound = ports
        .acquire(
            port,
            PortBinding::new(IpAddr::V4(LOCAL_A), false).unwrap(),
            false,
        )
        .unwrap();
    ports.release(other_address);
    ports.release(rebound);
}

#[test]
fn reuse_reclassification_is_released_from_the_new_class() {
    let mut ports = PortNamespace::new();
    let first = ports
        .acquire(
            8_082,
            PortBinding::new(IpAddr::V4(LOCAL_A), false).unwrap(),
            false,
        )
        .unwrap();
    let first = ports.set_reuse(first, true);
    let second = ports
        .acquire(
            8_082,
            PortBinding::new(IpAddr::V4(LOCAL_A), false).unwrap(),
            true,
        )
        .unwrap();

    ports.release(first);
    assert_eq!(
        ports.acquire(
            8_082,
            PortBinding::new(IpAddr::V4(LOCAL_A), false).unwrap(),
            false
        ),
        Err(PortError::AddressInUse)
    );
    ports.release(second);
    let exclusive = ports
        .acquire(
            8_082,
            PortBinding::new(IpAddr::V4(LOCAL_A), false).unwrap(),
            false,
        )
        .unwrap();
    ports.release(exclusive);
}

#[test]
fn accepted_connection_projects_wildcard_listener_to_exact_tuple() {
    let mut ports = PortNamespace::new();
    let listener = ports
        .acquire(
            8_083,
            PortBinding::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), false).unwrap(),
            false,
        )
        .unwrap();
    let listener = ports.claim_listener(listener).unwrap();
    let prepared = ports
        .prepare_retain_for_address(
            listener,
            PortBinding::new(IpAddr::V4(LOCAL_A), false).unwrap(),
        )
        .unwrap();
    let accepted = ports.commit_retained(prepared);

    ports.release(listener);
    assert_eq!(
        ports.acquire(
            8_083,
            PortBinding::new(IpAddr::V4(LOCAL_A), false).unwrap(),
            false
        ),
        Err(PortError::AddressInUse)
    );
    let other_address = ports
        .acquire(
            8_083,
            PortBinding::new(IpAddr::V4(LOCAL_B), false).unwrap(),
            false,
        )
        .unwrap();
    ports.release(other_address);
    ports.release(accepted);
}

#[test]
fn abandoned_accepted_preparation_does_not_publish_membership() {
    let mut ports = PortNamespace::new();
    let listener = ports
        .acquire(
            8_084,
            PortBinding::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), false).unwrap(),
            false,
        )
        .unwrap();
    let prepared = ports
        .prepare_retain_for_address(
            listener,
            PortBinding::new(IpAddr::V4(LOCAL_A), false).unwrap(),
        )
        .unwrap();
    drop(prepared);
    ports.release(listener);

    let rebound = ports
        .acquire(
            8_084,
            PortBinding::new(IpAddr::V4(LOCAL_A), false).unwrap(),
            false,
        )
        .unwrap();
    ports.release(rebound);
}

#[test]
fn active_connect_readdresses_wildcard_without_a_second_membership() {
    let mut ports = PortNamespace::new();
    let wildcard = ports
        .acquire(
            8_086,
            PortBinding::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), false).unwrap(),
            false,
        )
        .unwrap();
    let prepared = ports
        .prepare_readdress(
            wildcard,
            PortBinding::new(IpAddr::V4(LOCAL_A), false).unwrap(),
        )
        .unwrap();
    let connected = ports.commit_readdress(prepared);

    assert_eq!(
        ports.acquire(
            8_086,
            PortBinding::new(IpAddr::V4(LOCAL_A), false).unwrap(),
            false
        ),
        Err(PortError::AddressInUse)
    );
    let other_interface = ports
        .acquire(
            8_086,
            PortBinding::new(IpAddr::V4(LOCAL_B), false).unwrap(),
            false,
        )
        .unwrap();
    ports.release(other_interface);
    ports.release(connected);
}

#[test]
fn failed_connect_preparation_preserves_wildcard_membership() {
    let mut ports = PortNamespace::new();
    let wildcard = ports
        .acquire(
            8_087,
            PortBinding::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), false).unwrap(),
            false,
        )
        .unwrap();
    let prepared = ports
        .prepare_readdress(
            wildcard,
            PortBinding::new(IpAddr::V4(LOCAL_A), false).unwrap(),
        )
        .unwrap();
    drop(prepared);

    assert_eq!(
        ports.acquire(
            8_087,
            PortBinding::new(IpAddr::V4(LOCAL_B), false).unwrap(),
            false
        ),
        Err(PortError::AddressInUse)
    );
    ports.release(wildcard);
}

#[test]
fn listener_claims_reject_overlap_without_reuseport() {
    let mut ports = PortNamespace::new();
    let first = ports
        .acquire(
            8_085,
            PortBinding::new(IpAddr::V4(LOCAL_A), false).unwrap(),
            true,
        )
        .unwrap();
    let second = ports
        .acquire(
            8_085,
            PortBinding::new(IpAddr::V4(LOCAL_A), false).unwrap(),
            true,
        )
        .unwrap();
    let first = ports.claim_listener(first).unwrap();

    assert_eq!(ports.claim_listener(second), Err(PortError::AddressInUse));
    ports.release(second);
    ports.release(first);

    let local_a = ports
        .acquire(
            8_085,
            PortBinding::new(IpAddr::V4(LOCAL_A), false).unwrap(),
            true,
        )
        .unwrap();
    let local_b = ports
        .acquire(
            8_085,
            PortBinding::new(IpAddr::V4(LOCAL_B), false).unwrap(),
            true,
        )
        .unwrap();
    let local_a = ports.claim_listener(local_a).unwrap();
    let local_b = ports.claim_listener(local_b).unwrap();
    ports.release(local_a);
    ports.release(local_b);
}

#[test]
fn ephemeral_bitmap_exhaustion_and_release_are_exact() {
    let mut ports = PortNamespace::new();
    let count = usize::from(EPHEMERAL_END - EPHEMERAL_START) + 1;
    let mut leases = Vec::with_capacity(count);
    for expected in EPHEMERAL_START..=EPHEMERAL_END {
        let lease = ports
            .acquire_ephemeral(
                PortBinding::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), false).unwrap(),
                false,
            )
            .unwrap();
        assert_eq!(lease.port(), expected);
        leases.push(lease);
    }
    assert_eq!(
        ports.acquire_ephemeral(
            PortBinding::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), false).unwrap(),
            false
        ),
        Err(PortError::AddressInUse)
    );

    // IPv4 耗尽不能耗尽 V6ONLY；双栈通配则要求两族均无占用。
    let v6 = PortBinding::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), true).unwrap();
    let dual = PortBinding::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), false).unwrap();
    let v6_lease = ports.acquire_ephemeral(v6, false).unwrap();
    assert_eq!(v6_lease.port(), EPHEMERAL_START);
    assert_eq!(
        ports.acquire_ephemeral(dual, false),
        Err(PortError::AddressInUse)
    );
    let first = leases.swap_remove(0);
    ports.release(first);
    let reused_v4 = ports
        .acquire_ephemeral(
            PortBinding::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), false).unwrap(),
            false,
        )
        .unwrap();
    assert_eq!(reused_v4.port(), EPHEMERAL_START);
    assert_eq!(
        ports.acquire_ephemeral(dual, false),
        Err(PortError::AddressInUse)
    );
    let released = leases.swap_remove(73);
    let released_port = released.port();
    ports.release(released);
    let reused_dual = ports.acquire_ephemeral(dual, false).unwrap();
    assert_eq!(reused_dual.port(), released_port);
    ports.release(reused_dual);
    ports.release(reused_v4);
    ports.release(v6_lease);
    for lease in leases {
        ports.release(lease);
    }
    let dual = ports.acquire(EPHEMERAL_START, dual, false).unwrap();
    ports.release(dual);
}

#[test]
fn dual_family_bind_and_listener_overlap_matches_one_address_space() {
    let bindings = [
        PortBinding::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), false).unwrap(),
        PortBinding::new(IpAddr::V4(LOCAL_A), false).unwrap(),
        PortBinding::new(IpAddr::V4(LOCAL_B), false).unwrap(),
        PortBinding::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), true).unwrap(),
        PortBinding::new("2001:db8::1".parse().unwrap(), false).unwrap(),
        PortBinding::new("2001:db8::2".parse().unwrap(), true).unwrap(),
        PortBinding::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), false).unwrap(),
        PortBinding::new(IpAddr::V6(LOCAL_A.to_ipv6_mapped()), false).unwrap(),
    ];
    // 固定的地址空间交集表，按两种绑定顺序与双方 REUSEADDR 组合执行真实 owner。
    let overlaps = [
        [true, true, true, false, false, false, true, true],
        [true, true, false, false, false, false, true, true],
        [true, false, true, false, false, false, true, false],
        [false, false, false, true, true, true, true, false],
        [false, false, false, true, true, false, true, false],
        [false, false, false, true, false, true, true, false],
        [true, true, true, true, true, true, true, true],
        [true, true, false, false, false, false, true, true],
    ];
    let devices = [None, Some(2), Some(3)];
    let device_overlaps = [[true, true, true], [true, true, false], [true, false, true]];
    for (d1, first_device) in devices.into_iter().enumerate() {
        for (d2, second_device) in devices.into_iter().enumerate() {
            let devices_overlap = device_overlaps[d1][d2];
            for (a, first) in bindings.into_iter().enumerate() {
                for (b, second) in bindings.into_iter().enumerate() {
                    let first = first.with_interface(first_device);
                    let second = second.with_interface(second_device);
                    for (first_reuse, second_reuse) in
                        [(false, false), (false, true), (true, false), (true, true)]
                    {
                        let mut ports = PortNamespace::new();
                        let lease = ports.acquire(8_091, first, first_reuse).unwrap();
                        let result = ports.acquire(8_091, second, second_reuse);
                        if (overlaps[a][b] && devices_overlap) && !(first_reuse && second_reuse) {
                            assert_eq!(result, Err(PortError::AddressInUse), "{a}, {b}");
                        } else {
                            ports.release(result.unwrap());
                        }
                        ports.release(lease);
                        let cleared = ports.acquire(8_091, bindings[6], false).unwrap();
                        ports.release(cleared);
                    }
                    let mut ports = PortNamespace::new();
                    let first_lease = ports.acquire(8_091, first, true).unwrap();
                    let second_lease = ports.acquire(8_091, second, true).unwrap();
                    let first_lease = ports.claim_listener(first_lease).unwrap();
                    let result = ports.claim_listener(second_lease);
                    if overlaps[a][b] && devices_overlap {
                        assert_eq!(result, Err(PortError::AddressInUse), "listener {a}, {b}");
                        ports.release(second_lease);
                    } else {
                        ports.release(result.unwrap());
                    }
                    let late_bind = ports.acquire(8_091, second, true);
                    if overlaps[a][b] && devices_overlap {
                        assert_eq!(
                            late_bind,
                            Err(PortError::AddressInUse),
                            "bind after listen {a}, {b}"
                        );
                    } else {
                        ports.release(late_bind.unwrap());
                    }
                    ports.release(first_lease);
                }
            }
        }
    }
    assert_eq!(
        PortBinding::new(IpAddr::V6(LOCAL_A.to_ipv6_mapped()), true),
        Err(PortError::InvalidBinding)
    );
    assert_eq!(
        PortBinding::new(IpAddr::V6(Ipv4Addr::UNSPECIFIED.to_ipv6_mapped()), false).unwrap(),
        bindings[0]
    );
}

#[test]
fn dual_family_retention_refinement_and_inverse_preserve_exact_membership() {
    let dual = PortBinding::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), false).unwrap();
    let v6 = PortBinding::new("2001:db8::1".parse().unwrap(), true).unwrap();
    let mut ports = PortNamespace::new();
    let listener = ports.acquire(8_092, dual, false).unwrap();
    let listener = ports.claim_listener(listener).unwrap();
    let prepared = ports
        .prepare_retain_for_address(
            listener,
            PortBinding::new(IpAddr::V4(LOCAL_A), false).unwrap(),
        )
        .unwrap();
    let child = ports.commit_retained(prepared);
    ports.release(listener);
    let other_family = ports.acquire(8_092, v6, false).unwrap();
    assert_eq!(
        ports.acquire(
            8_092,
            PortBinding::new(IpAddr::V4(LOCAL_A), false).unwrap(),
            false
        ),
        Err(PortError::AddressInUse)
    );
    ports.release(other_family);
    ports.release(child);

    let original = ports.acquire_ephemeral(dual, false).unwrap();
    let prepared = ports.prepare_readdress(original, v6).unwrap();
    let exact = ports.commit_readdress(prepared);
    let other_family = ports
        .acquire_ephemeral(
            PortBinding::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), false).unwrap(),
            false,
        )
        .unwrap();
    assert_eq!(exact.port(), other_family.port());
    ports.release(other_family);
    let restored = ports.restore_bound_address(exact, dual);
    assert_eq!(
        ports.acquire(
            restored.port(),
            PortBinding::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), false).unwrap(),
            false
        ),
        Err(PortError::AddressInUse)
    );
    let restored = ports.set_reuse(restored, true);
    let shared = ports.acquire(restored.port(), v6, true).unwrap();
    ports.release(restored);
    assert_eq!(
        ports.acquire(shared.port(), dual, false),
        Err(PortError::AddressInUse)
    );
    ports.release(shared);
}

#[test]
fn dual_family_allocation_failure_does_not_publish_partial_membership() {
    let binding = PortBinding::new("2001:db8::1".parse().unwrap(), true).unwrap();
    let dual = PortBinding::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), false).unwrap();
    for successful in 0..2 {
        let mut ports = PortNamespace::new();
        crate::allocation_failure::fail_after(successful);
        let result = ports.acquire(EPHEMERAL_START, binding, false);
        assert!(crate::allocation_failure::was_consumed());
        assert_eq!(result, Err(PortError::NoMemory));
        let admitted = ports.acquire_ephemeral(dual, false).unwrap();
        assert_eq!(admitted.port(), EPHEMERAL_START);
        ports.release(admitted);
    }
    let mut ports = PortNamespace::new();
    let original = ports.acquire(EPHEMERAL_START, dual, false).unwrap();
    crate::allocation_failure::fail_next();
    let result = ports.prepare_readdress(original, binding);
    assert!(crate::allocation_failure::was_consumed());
    assert!(matches!(result, Err(PortError::NoMemory)));
    assert_eq!(
        ports.acquire(EPHEMERAL_START, binding, false),
        Err(PortError::AddressInUse)
    );
    let prepared = ports.prepare_readdress(original, binding).unwrap();
    crate::allocation_failure::fail_next();
    let exact = ports.commit_readdress(prepared);
    let restored = ports.restore_bound_address(exact, dual);
    let restored = ports.claim_listener(restored).unwrap();
    let restored = ports.set_reuse(restored, true);
    ports.release(restored);
    assert!(
        !crate::allocation_failure::was_consumed(),
        "publication and inverse must not allocate"
    );
}

#[test]
fn device_rebind_preserves_children_and_clearing_scope_needs_no_allocation() {
    let mut ports = PortNamespace::new();
    let scoped = PortBinding::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), false)
        .unwrap()
        .with_interface(Some(2));
    let exact = PortBinding::new(IpAddr::V4(LOCAL_A), false)
        .unwrap()
        .with_interface(Some(2));
    let parent = ports.acquire(8_093, scoped, false).unwrap();
    let parent = ports.claim_listener(parent).unwrap();
    let prepared = ports.prepare_retain_for_address(parent, exact).unwrap();
    let child = ports.commit_retained(prepared);
    let other = scoped.with_interface(Some(3));
    let prepared = ports.prepare_rebind(parent, other).unwrap();
    let parent = ports.commit_readdress(prepared);
    assert_eq!(
        ports.acquire(8_093, exact, false),
        Err(PortError::AddressInUse)
    );
    let reusable_address = PortBinding::new(IpAddr::V4(LOCAL_B), false)
        .unwrap()
        .with_interface(Some(2));
    let free = ports.acquire(8_093, reusable_address, false).unwrap();
    ports.release(free);
    assert_eq!(
        ports.acquire(8_093, other, true),
        Err(PortError::AddressInUse)
    );
    ports.release(parent);
    crate::allocation_failure::fail_next();
    let child = ports.restore_bound_address(child, exact.with_interface(None));
    assert!(
        !crate::allocation_failure::was_consumed(),
        "clearing scope must reuse global exact node"
    );
    assert_eq!(
        ports.acquire(8_093, exact.with_interface(Some(3)), false),
        Err(PortError::AddressInUse)
    );
    ports.release(child);
    let free = ports
        .acquire(
            8_093,
            PortBinding::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), false).unwrap(),
            false,
        )
        .unwrap();
    ports.release(free);
}

#[test]
fn scoped_binding_preparation_is_atomic_at_every_fallible_node() {
    let binding = PortBinding::new(IpAddr::V4(LOCAL_A), false)
        .unwrap()
        .with_interface(Some(2));
    // 外层端口、全局精确地址、设备与设备精确地址四个节点都可能独立 OOM。
    for successful in 0..4 {
        let mut ports = PortNamespace::new();
        crate::allocation_failure::fail_after(successful);
        assert_eq!(
            ports.acquire(8_094, binding, false),
            Err(PortError::NoMemory)
        );
        assert!(crate::allocation_failure::was_consumed());
        let free = ports
            .acquire(
                8_094,
                PortBinding::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), false).unwrap(),
                false,
            )
            .unwrap();
        ports.release(free);
    }
    // 原无接口精确节点在迁移失败后仍存活，只有两个设备节点需要新分配。
    for successful in 0..2 {
        let mut ports = PortNamespace::new();
        let original = ports
            .acquire(8_094, binding.with_interface(None), false)
            .unwrap();
        crate::allocation_failure::fail_after(successful);
        assert!(matches!(
            ports.prepare_rebind(original, binding),
            Err(PortError::NoMemory)
        ));
        assert!(crate::allocation_failure::was_consumed());
        assert_eq!(
            ports.acquire(8_094, binding.with_interface(Some(3)), false),
            Err(PortError::AddressInUse)
        );
        ports.release(original);
        let free = ports
            .acquire(
                8_094,
                PortBinding::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), false).unwrap(),
                false,
            )
            .unwrap();
        ports.release(free);
    }
}
