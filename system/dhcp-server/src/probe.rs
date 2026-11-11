//! Conflict detection for new DHCPv4 offers on the pool's directly attached link.

use core::net::Ipv4Addr;

use packet_wire::{
    PacketError,
    arp::{ArpHeader, ArpPacket},
    dhcpv4::MessageType,
};

use super::{Action, DATAGRAM_BYTES, Error, Phase, Reply, Request, Server};

/// Enough space for an Ethernet/IPv4 ARP probe.
pub const ARP_BYTES: usize = 28;

// RFC 2131 server-side check using an RFC 5227 probe. Bound the outstanding transactions so a
// DISCOVER burst cannot retain unbounded request bodies during the observation interval.
const CHECK_MS: u64 = 1_000;
const MAX_CHECKS: usize = 64;

pub(super) struct Check {
    address: u32,
    request: Request,
    ready_ms: Option<u64>,
}

/// Encode a broadcast ARP probe for `address` using the interface's `mac`.
/// # Errors
/// The packet encoder rejected the fixed Ethernet/IPv4 layout.
pub fn arp_probe(
    address: Ipv4Addr,
    mac: [u8; 6],
    output: &mut [u8; ARP_BYTES],
) -> Result<(), PacketError> {
    ArpHeader {
        hardware_type: 1,
        protocol_type: 0x0800,
        operation: 1,
        sender_hardware: &mac,
        sender_protocol: &[0; 4],
        target_hardware: &[0; 6],
        target_protocol: &address.octets(),
    }
    .emit(output)?;
    Ok(())
}

impl Server {
    pub(super) fn discover<'a>(
        &mut self,
        request: Request,
        now: u64,
        output: &'a mut [u8; DATAGRAM_BYTES],
    ) -> Result<Option<Action<'a>>, Error> {
        let known = self.leases.lookup(request.client());
        if let Some((address, Phase::Checking)) = known {
            let check = self
                .checks
                .iter_mut()
                .find(|check| check.address == address)
                .expect("a checking lease owns an active probe");
            check.request = request;
            return Ok(None);
        }
        let check = known.is_none() && self.pool.options.probe_addresses;
        let phase = if check {
            if self.checks.len() == MAX_CHECKS {
                return Ok(None);
            }
            self.checks.try_reserve(1).map_err(|_| Error::NoMemory)?;
            Phase::Checking
        } else {
            Phase::Offered(request.xid)
        };
        let Some(address) = self.leases.offer(
            request.client(),
            request.mac,
            phase,
            request.requested.map(u32::from),
            now,
        )?
        else {
            return Ok(None);
        };
        if check {
            self.checks.push(Check {
                address,
                request,
                ready_ms: None,
            });
            return Ok(Some(Action::Probe(address.into())));
        }
        self.datagram(&request, MessageType::Offer, address.into(), output)
            .map(Action::Reply)
            .map(Some)
    }

    /// Start the observation interval after successful transmission at `sent_ms`.
    /// Failed transmission releases the address without sending an offer.
    pub fn probe_transmitted(&mut self, address: Ipv4Addr, sent_ms: Option<u64>) {
        let address = u32::from(address);
        if let Some(index) = self
            .checks
            .iter()
            .position(|check| check.address == address)
        {
            if let Some(now) = sent_ms {
                self.checks[index].ready_ms = Some(now.saturating_add(CHECK_MS));
            } else {
                self.checks.swap_remove(index);
                self.leases.remove(address);
            }
        }
    }

    /// Cancel in-flight probes before pausing or changing the interface.
    pub fn cancel_checks(&mut self) {
        for check in self.checks.drain(..) {
            self.leases.remove(check.address);
        }
    }

    /// Return the next time a successfully transmitted probe can finish.
    pub fn next_check(&self) -> Option<u64> {
        self.checks.iter().filter_map(|check| check.ready_ms).min()
    }

    /// Observe ARP on the pool's link and reserve another address if this one is occupied.
    /// # Errors
    /// Malformed ARP or insufficient resources to prepare a replacement offer.
    pub fn observe_arp<'a>(
        &mut self,
        bytes: &[u8],
        source: [u8; 6],
        local: [u8; 6],
        now_ms: u64,
        output: &'a mut [u8; DATAGRAM_BYTES],
    ) -> Result<Option<Action<'a>>, Error> {
        if self.checks.is_empty() || source == local || source == [0; 6] || source[0] & 1 != 0 {
            return Ok(None);
        }
        let packet = ArpPacket::parse(bytes)?;
        if packet.hardware_type() != 1
            || packet.protocol_type() != 0x0800
            || !matches!(packet.operation(), 1 | 2)
            || packet.sender_hardware() != source
        {
            return Ok(None);
        }
        let candidate = if packet.operation() == 1 && packet.sender_protocol() == [0; 4] {
            packet.target_protocol()
        } else {
            packet.sender_protocol()
        };
        let Ok(candidate) = <[u8; 4]>::try_from(candidate) else {
            return Ok(None);
        };
        let address = u32::from_be_bytes(candidate);
        let Some(index) = self
            .checks
            .iter()
            .position(|check| check.address == address)
        else {
            return Ok(None);
        };
        let check = self.checks.swap_remove(index);
        self.leases.decline(address, source, now_ms);
        self.expire(now_ms);
        self.discover(check.request, now_ms, output)
    }

    /// Complete one elapsed probe after the adapter has drained pending ARP input.
    /// # Errors
    /// The reply could not be encoded.
    pub fn finish_check<'a>(
        &mut self,
        now_ms: u64,
        output: &'a mut [u8; DATAGRAM_BYTES],
    ) -> Result<Option<Reply<'a>>, Error> {
        let Some(index) = self
            .checks
            .iter()
            .position(|check| check.ready_ms.is_some_and(|ready| ready <= now_ms))
        else {
            return Ok(None);
        };
        let check = self.checks.swap_remove(index);
        let request = check.request;
        self.leases
            .checked(check.address, request.mac, request.xid, now_ms);
        self.datagram(&request, MessageType::Offer, check.address.into(), output)
            .map(Some)
    }
}
