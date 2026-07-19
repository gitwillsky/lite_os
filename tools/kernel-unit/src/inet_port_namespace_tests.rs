use core::net::Ipv4Addr;

use crate::inet_port_namespace::{EPHEMERAL_END, EPHEMERAL_START, PortError, PortNamespace};

const LOCAL_A: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 15);
const LOCAL_B: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 16);

#[test]
fn exact_addresses_share_a_port_but_same_address_conflicts() {
    let mut ports = PortNamespace::new();
    let first = ports.acquire(8_080, Some(LOCAL_A), false).unwrap();
    let second = ports.acquire(8_080, Some(LOCAL_B), false).unwrap();

    assert_eq!(
        ports.acquire(8_080, Some(LOCAL_A), false),
        Err(PortError::AddressInUse)
    );
    ports.release(first);
    ports.release(second);
}

#[test]
fn wildcard_and_exact_memberships_obey_reuse_policy() {
    let mut ports = PortNamespace::new();
    let wildcard = ports.acquire(8_081, None, true).unwrap();
    let exact = ports.acquire(8_081, Some(LOCAL_A), true).unwrap();

    assert_eq!(
        ports.acquire(8_081, None, false),
        Err(PortError::AddressInUse)
    );
    assert_eq!(
        ports.acquire(8_081, Some(LOCAL_A), false),
        Err(PortError::AddressInUse)
    );
    ports.release(wildcard);
    ports.release(exact);

    let exclusive = ports.acquire(8_081, Some(LOCAL_A), false).unwrap();
    assert_eq!(
        ports.acquire(8_081, None, true),
        Err(PortError::AddressInUse)
    );
    ports.release(exclusive);
}

#[test]
fn specific_port_zero_lease_keeps_its_exact_address() {
    let mut ports = PortNamespace::new();
    let specific = ports.acquire_ephemeral(Some(LOCAL_A), false).unwrap();
    let port = specific.port();
    let other_address = ports.acquire(port, Some(LOCAL_B), false).unwrap();

    assert_eq!(
        ports.acquire(port, Some(LOCAL_A), false),
        Err(PortError::AddressInUse)
    );
    ports.release(specific);
    let rebound = ports.acquire(port, Some(LOCAL_A), false).unwrap();
    ports.release(other_address);
    ports.release(rebound);
}

#[test]
fn reuse_reclassification_is_released_from_the_new_class() {
    let mut ports = PortNamespace::new();
    let first = ports.acquire(8_082, Some(LOCAL_A), false).unwrap();
    let first = ports.set_reuse(first, true);
    let second = ports.acquire(8_082, Some(LOCAL_A), true).unwrap();

    ports.release(first);
    assert_eq!(
        ports.acquire(8_082, Some(LOCAL_A), false),
        Err(PortError::AddressInUse)
    );
    ports.release(second);
    let exclusive = ports.acquire(8_082, Some(LOCAL_A), false).unwrap();
    ports.release(exclusive);
}

#[test]
fn accepted_connection_projects_wildcard_listener_to_exact_tuple() {
    let mut ports = PortNamespace::new();
    let listener = ports.acquire(8_083, None, false).unwrap();
    let listener = ports.claim_listener(listener).unwrap();
    let prepared = ports.prepare_retain_for_address(listener, LOCAL_A).unwrap();
    let accepted = ports.commit_retained(prepared);

    ports.release(listener);
    assert_eq!(
        ports.acquire(8_083, Some(LOCAL_A), false),
        Err(PortError::AddressInUse)
    );
    let other_address = ports.acquire(8_083, Some(LOCAL_B), false).unwrap();
    ports.release(other_address);
    ports.release(accepted);
}

#[test]
fn abandoned_accepted_preparation_does_not_publish_membership() {
    let mut ports = PortNamespace::new();
    let listener = ports.acquire(8_084, None, false).unwrap();
    let prepared = ports.prepare_retain_for_address(listener, LOCAL_A).unwrap();
    drop(prepared);
    ports.release(listener);

    let rebound = ports.acquire(8_084, Some(LOCAL_A), false).unwrap();
    ports.release(rebound);
}

#[test]
fn active_connect_readdresses_wildcard_without_a_second_membership() {
    let mut ports = PortNamespace::new();
    let wildcard = ports.acquire(8_086, None, false).unwrap();
    let prepared = ports.prepare_readdress(wildcard, LOCAL_A).unwrap();
    let connected = ports.commit_readdress(prepared);

    assert_eq!(
        ports.acquire(8_086, Some(LOCAL_A), false),
        Err(PortError::AddressInUse)
    );
    let other_interface = ports.acquire(8_086, Some(LOCAL_B), false).unwrap();
    ports.release(other_interface);
    ports.release(connected);
}

#[test]
fn failed_connect_preparation_preserves_wildcard_membership() {
    let mut ports = PortNamespace::new();
    let wildcard = ports.acquire(8_087, None, false).unwrap();
    let prepared = ports.prepare_readdress(wildcard, LOCAL_A).unwrap();
    drop(prepared);

    assert_eq!(
        ports.acquire(8_087, Some(LOCAL_B), false),
        Err(PortError::AddressInUse)
    );
    ports.release(wildcard);
}

#[test]
fn listener_claims_reject_overlap_without_reuseport() {
    let mut ports = PortNamespace::new();
    let first = ports.acquire(8_085, Some(LOCAL_A), true).unwrap();
    let second = ports.acquire(8_085, Some(LOCAL_A), true).unwrap();
    let first = ports.claim_listener(first).unwrap();

    assert_eq!(ports.claim_listener(second), Err(PortError::AddressInUse));
    ports.release(second);
    ports.release(first);

    let local_a = ports.acquire(8_085, Some(LOCAL_A), true).unwrap();
    let local_b = ports.acquire(8_085, Some(LOCAL_B), true).unwrap();
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
        let lease = ports.acquire_ephemeral(None, false).unwrap();
        assert_eq!(lease.port(), expected);
        leases.push(lease);
    }
    assert_eq!(
        ports.acquire_ephemeral(None, false),
        Err(PortError::AddressInUse)
    );

    let released = leases.swap_remove(73);
    let released_port = released.port();
    ports.release(released);
    let reused = ports.acquire_ephemeral(None, false).unwrap();
    assert_eq!(reused.port(), released_port);
    ports.release(reused);
    for lease in leases {
        ports.release(lease);
    }
}
