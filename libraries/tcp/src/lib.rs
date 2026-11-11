//! TCP connection mechanism independent of interfaces, devices and operating-system socket policy.
//!
//! The state machine and storage algorithms derive from smoltcp 0.13.1 at
//! e347a1e2d3ac33c5ce2c0c114e24b85ae23c4897, under the included 0BSD license.
//! Packet fields are captured and emitted exclusively through packet-wire. The caller owns
//! routing, address qualification, entropy, clocks, buffer allocation and output completion.
#![no_std]
extern crate alloc;

macro_rules! net_trace { ($($arg:tt)*) => { if false { let _ = core::format_args!($($arg)*); } }; }
macro_rules! net_debug { ($($arg:tt)*) => { net_trace!($($arg)*) }; }

mod authentication;
mod congestion;
pub use authentication::Md5Key;
mod loss;
mod reassembly;
mod segment;
mod sequence;
mod socket;
mod stream_buffer;
pub mod time;
use core::net::IpAddr;
pub use segment::{Control, IncomingSegment, IpPacket, Segment, Timestamp, TimestampGenerator};
pub use sequence::Sequence;
pub use socket::{
    ConnectError, FailureReason, ListenError, OutputFailure, RecvError, SendError, Socket,
    SocketBuffer, State,
};

/// Next protocol work; no timer is needed while only external input can advance the connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum PollAt {
    Now,
    Time(time::Instant),
    Ingress,
}
/// Immutable transport metrics selected by the caller's authoritative route owner.
#[derive(Clone, Copy, Debug)]
pub struct TransportRoute {
    pub advertised_mss: usize,
    pub receive_window: usize,
    /// Route RTT seed in milliseconds; zero leaves the normal initial RTO unchanged.
    pub initial_rtt: u32,
}
/// Per-operation path facts, clock and entropy. Implementations must use one frozen route decision.
/// No interface table or next-hop cache belongs to the connection mechanism.
pub trait Context {
    fn now(&self) -> time::Instant;
    fn ip_mtu(&self) -> usize;
    fn interface_index(&self) -> u32;
    fn transport_route(&self) -> Option<TransportRoute>;
    /// Select an eligible local source from the same route decision, or return None.
    fn source_address(&self, destination: IpAddr) -> Option<IpAddr>;
    /// RFC 9293 initial sequence number for the exact tuple, incorporating a clock and a secret PRF.
    /// Called only when opening; the caller owns the secret and must not reuse a test-only constant.
    fn initial_sequence(&mut self, local: Endpoint, remote: Endpoint) -> Sequence;
}
/// An exact transport tuple endpoint, separate from the caller's original bind identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Endpoint {
    pub addr: IpAddr,
    pub port: u16,
}
impl Endpoint {
    pub const fn new(addr: IpAddr, port: u16) -> Self {
        Self { addr, port }
    }
}
impl core::fmt::Display for Endpoint {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        core::net::SocketAddr::new(self.addr, self.port).fmt(f)
    }
}
impl From<(IpAddr, u16)> for Endpoint {
    fn from((addr, port): (IpAddr, u16)) -> Self {
        Self { addr, port }
    }
}
/// A protocol listener; None accepts any local address admitted by the caller's IP layer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ListenEndpoint {
    pub addr: Option<IpAddr>,
    pub port: u16,
}
impl From<u16> for ListenEndpoint {
    fn from(port: u16) -> Self {
        Self { addr: None, port }
    }
}
impl From<Endpoint> for ListenEndpoint {
    fn from(endpoint: Endpoint) -> Self {
        Self {
            addr: Some(endpoint.addr),
            port: endpoint.port,
        }
    }
}

#[cfg(test)]
mod tests;
