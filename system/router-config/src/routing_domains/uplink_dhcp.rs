//! Project upstream DHCPv4 lease events onto the domain's interface transaction.

use super::*;

impl RouterConfig {
    /// Apply a lease event to the attached interface whose current session owns `key`.
    pub(in super::super) fn domain_lease_ready(&self, inner: &mut Inner, key: u64) {
        for scope in &mut inner.domains.scopes {
            let Some(port) = scope.interfaces.iter_mut().find(|port| {
                port.dhcp
                    .as_ref()
                    .is_some_and(|(current, _)| *current == key)
            }) else {
                continue;
            };
            let domain = inner
                .saved
                .as_ref()
                .and_then(|saved| saved.routing_domains.get(&scope.name));
            // A commit may have removed this uplink while an old ACK was queued.
            if scope.retired || !domain.is_some_and(|domain| port.wants_dhcp(domain)) {
                port.dhcp = None;
                port.lease = None;
                return;
            }
            let profile = port
                .profile(domain.expect("current routing domain"))
                .expect("configured DHCP interface");
            let (_, events) = port.dhcp.as_ref().expect("matched DHCP session");
            let event = leases::read_event(events, &self.port, key);
            let (result, reply) = match event {
                Ok((event, reply)) => {
                    let lease = match event {
                        LeaseEvent::Lost => None,
                        LeaseEvent::Bound {
                            address,
                            prefix_length,
                            gateway,
                            timing,
                            ..
                        } => Some(DomainLease {
                            address,
                            prefix_length,
                            gateway,
                            timing,
                        }),
                    };
                    let address = lease.as_ref().map(|lease| lease.address);
                    let prefix = lease.as_ref().map_or(0, |lease| lease.prefix_length);
                    let route = lease.as_ref().and_then(|lease| {
                        domain_default_route(profile, lease.gateway, RTPROT_DHCP)
                    });
                    let now = native_runtime::system::monotonic();
                    let unchanged = port
                        .lease
                        .as_ref()
                        .filter(|lease| lease.valid_at(now))
                        .zip(lease.as_ref())
                        .is_some_and(|(old, new)| {
                            (old.address, old.prefix_length, old.gateway)
                                == (new.address, new.prefix_length, new.gateway)
                        });
                    let result = if unchanged {
                        Ok(())
                    } else {
                        leases::apply_address(&scope.controller, port.index, address, prefix, route)
                    };
                    if result.is_ok() {
                        port.lease = lease;
                    }
                    (result, reply)
                }
                Err(native_abi::ERR_SHOULD_WAIT) => return,
                Err(status) => (Err(status), None),
            };
            if result.is_err() {
                port.dhcp = None;
                port.lease = None;
                scope.pending = true;
                self.queue_domains(inner);
            }
            Self::changed(inner);
            if let Some(reply) = reply {
                let _ = reply.write(
                    &result.err().unwrap_or(native_abi::OK).to_le_bytes(),
                    Vec::new(),
                );
            }
            return;
        }
    }
}
