// Explicit remote-protocol construction seam for policy-only host fixtures. The actual
// NetNamespace still owns identity, quota and lifetime; native channel traffic runs in guest.
#[allow(unused_imports, reason = "only message-limit consumers need this wire constant")]
pub use network_wire::MAX_IPV4_UDP_BYTES;
pub struct RemoteNamespace;
