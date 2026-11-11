//! RFC 2385 digest calculation and verification. Key selection and lifetime belong to the caller;
//! no per-packet allocation, key negotiation, global secret or connection state lives here.
use alloc::{collections::TryReserveError, vec::Vec};
use core::{fmt, net::IpAddr};
use md5::{Digest, Md5};
use packet_wire::{
    PacketError,
    tcp::{TcpPacket, options::TcpOption},
};
use subtle::ConstantTimeEq;
use zeroize::Zeroize;

/// An owned secret, redacted from diagnostics and erased when its last owner retires.
/// A protocol adapter may enforce its own key length policy; TCP MD5 has no negotiated length.
pub struct Md5Key(Vec<u8>);
impl fmt::Debug for Md5Key {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Md5Key([redacted])")
    }
}

impl Drop for Md5Key {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}
impl Md5Key {
    /// Capture `bytes` in a unique, zeroized-on-retirement allocation.
    /// # Errors
    /// Allocation failure; no partially initialized key escapes.
    pub fn try_new(bytes: &[u8]) -> Result<Self, TryReserveError> {
        let mut key = Vec::new();
        key.try_reserve_exact(bytes.len())?;
        key.extend_from_slice(bytes);
        Ok(Self(key))
    }
    /// Verify the sole MD5 option in `packet` against the actual enclosing IP addresses.
    /// Returns false for missing signatures, wrong families/lengths or a nonmatching MAC.
    /// The IP owner must already have validated packet completeness and checksum/offload evidence.
    pub fn verify(&self, packet: TcpPacket<'_>, source: IpAddr, destination: IpAddr) -> bool {
        let received = packet.options().find_map(|option| match option.value {
            TcpOption::Md5(value) => Some(value),
            _ => None,
        });
        let (Some(received), Ok(expected)) = (received, self.digest(packet, source, destination))
        else {
            return false;
        };
        bool::from(expected.ct_eq(received))
    }
    pub(crate) fn digest(
        &self,
        packet: TcpPacket<'_>,
        source: IpAddr,
        destination: IpAddr,
    ) -> Result<[u8; 16], PacketError> {
        let mut hash = Md5::new();
        match (source, destination) {
            (IpAddr::V4(source), IpAddr::V4(destination)) => {
                let length =
                    u16::try_from(packet.bytes().len()).map_err(|_| PacketError::Capacity)?;
                hash.update(source.octets());
                hash.update(destination.octets());
                hash.update([0, 6]);
                hash.update(length.to_be_bytes());
            }
            (IpAddr::V6(source), IpAddr::V6(destination)) => {
                let length =
                    u32::try_from(packet.bytes().len()).map_err(|_| PacketError::Capacity)?;
                hash.update(source.octets());
                hash.update(destination.octets());
                hash.update(length.to_be_bytes());
                hash.update([0, 0, 0, 6]);
            }
            _ => return Err(PacketError::Malformed),
        }
        // The original Data Offset and flags participate; all options and only the checksum are excluded.
        hash.update(&packet.bytes()[..16]);
        hash.update([0, 0]);
        hash.update(&packet.bytes()[18..20]);
        hash.update(packet.payload());
        hash.update(&self.0);
        Ok(hash.finalize().into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // Independent Python hashlib vectors over the RFC pseudo-header, fixed header, data and key.
    // A sign/verify round trip alone would miss a shared mistake about options or segment length.
    #[test]
    fn md5_vectors_include_original_fixed_bits_and_pseudo_header_but_exclude_options_and_checksum()
    {
        for (source, destination, expected) in [
            (
                "192.0.2.1",
                "198.51.100.2",
                [
                    0x52, 0xc8, 0x57, 0xfe, 0x33, 0x79, 0x97, 0x91, 0x30, 0xc0, 0xd2, 0x58, 0x1a,
                    0xa7, 0xaf, 0xc9,
                ],
            ),
            (
                "2001:db8::1",
                "2001:db8::2",
                [
                    0x06, 0x5d, 0x4e, 0x94, 0x59, 0x93, 0x91, 0xae, 0xfa, 0x81, 0xcb, 0x16, 0x2b,
                    0xdc, 0x50, 0x54,
                ],
            ),
        ] {
            let source = source.parse().unwrap();
            let destination = destination.parse().unwrap();
            let key = Md5Key::try_new(b"fixture-key").unwrap();
            let mut bytes = [0; 48];
            bytes[..20].copy_from_slice(&[
                0, 179, 0x9c, 0x40, 1, 2, 3, 4, 0xa0, 0xb0, 0xc0, 0xd0, 0xa0, 0x10, 0x12, 0x34, 0,
                0, 0x56, 0x78,
            ]);
            bytes[20..22].copy_from_slice(&[19, 18]);
            bytes[40..].copy_from_slice(b"bgp-auth");
            assert_eq!(
                key.digest(
                    TcpPacket::parse_header(&bytes).unwrap(),
                    source,
                    destination
                ),
                Ok(expected)
            );
            bytes[22..38].copy_from_slice(&expected);
            assert!(key.verify(
                TcpPacket::parse_header(&bytes).unwrap(),
                source,
                destination
            ));
            bytes[16..18].copy_from_slice(&[0xab, 0xcd]);
            bytes[38..40].copy_from_slice(&[1, 1]);
            assert!(key.verify(
                TcpPacket::parse_header(&bytes).unwrap(),
                source,
                destination
            ));
            bytes[47] ^= 1;
            assert!(!key.verify(
                TcpPacket::parse_header(&bytes).unwrap(),
                source,
                destination
            ));
        }
    }
}
