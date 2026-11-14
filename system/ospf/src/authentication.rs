//! RFC 5709 和 RFC 7166 的 HMAC-SHA-256，认证在邻接状态机之前执行。

use core::net::IpAddr;
use fallible_tree::FallibleMap;
use hmac::{Hmac, KeyInit, Mac};
use native_runtime::{
    native_abi::{self, Status},
    system::monotonic,
};
use packet_wire::ospf::{HeaderFields, Kind, Packet, Version};
use router_protocol::ospf::{Authentication as Configuration, AuthenticationFailure as Failure};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

const DIGEST_LEN: usize = 32;
const TRAILER_HEADER: usize = 16;
const AT: u32 = 0x0400;
const PROTOCOL_ID: [u8; 2] = [0, 1];
const APAD: [u8; DIGEST_LEN] = [
    0x87, 0x8f, 0xe1, 0xf3, 0x87, 0x8f, 0xe1, 0xf3, 0x87, 0x8f, 0xe1, 0xf3, 0x87, 0x8f, 0xe1, 0xf3,
    0x87, 0x8f, 0xe1, 0xf3, 0x87, 0x8f, 0xe1, 0xf3, 0x87, 0x8f, 0xe1, 0xf3, 0x87, 0x8f, 0xe1, 0xf3,
];

type Sha256Mac = Hmac<Sha256>;

#[derive(Default)]
pub(super) struct SendSequence {
    epoch: Option<u32>,
    sent: u32,
}

impl SendSequence {
    pub(super) fn new(epoch: Option<u32>) -> Self {
        Self { epoch, sent: 0 }
    }

    fn next_v3(&mut self) -> Result<u64, Status> {
        let epoch = self.epoch.ok_or(native_abi::ERR_BAD_STATE)?;
        // 用尽时重建实例，由配置服务同步保留下一段序列，再恢复发送。
        self.sent = self
            .sent
            .checked_add(1)
            .ok_or(native_abi::ERR_OUT_OF_RANGE)?;
        Ok(u64::from(epoch) << 32 | u64::from(self.sent))
    }
}

#[derive(Default)]
struct Replay {
    accepted: [Option<u64>; 5],
}

pub(super) struct Authentication {
    send_id: u16,
    keys: FallibleMap<u16, Sha256Mac>,
    v2_received: Option<u32>,
    // 按 Router ID 和报文类型记高水位，v3 邻接超时或接口重绑定时仍拒绝旧报文。
    v3_received: FallibleMap<u32, Replay>,
}

impl Authentication {
    pub(super) fn new(version: Version, configuration: Configuration) -> Result<Self, Status> {
        let mut keys = FallibleMap::default();
        for (id, secret) in configuration.keys {
            let secret = secret.as_bytes();
            let protocol = if version == Version::V3 {
                &PROTOCOL_ID[..]
            } else {
                &[]
            };
            let mut key = Zeroizing::new([0u8; DIGEST_LEN]);
            let length = secret.len() + protocol.len();
            if length > DIGEST_LEN {
                let mut sha = Sha256::new();
                sha.update(secret);
                sha.update(protocol);
                key.copy_from_slice(&sha.finalize());
            } else {
                key[..secret.len()].copy_from_slice(secret);
                key[secret.len()..length].copy_from_slice(protocol);
            }
            let mac =
                Sha256Mac::new_from_slice(key.as_ref()).map_err(|_| native_abi::ERR_BAD_STATE)?;
            keys.try_insert(id, mac)
                .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        }
        Ok(Self {
            send_id: configuration.send_id,
            keys,
            v2_received: None,
            v3_received: FallibleMap::default(),
        })
    }

    pub(super) const fn overhead(version: Version) -> usize {
        DIGEST_LEN
            + if matches!(version, Version::V3) {
                TRAILER_HEADER
            } else {
                0
            }
    }

    pub(super) fn sign(
        &self,
        version: Version,
        kind: Kind,
        source: IpAddr,
        sequence: &mut SendSequence,
        length: usize,
        frame: &mut [u8],
    ) -> Result<(), Status> {
        let (base, tail) = frame.split_at_mut(length);
        let mut mac = self
            .keys
            .get(&self.send_id)
            .expect("validated send key")
            .clone();
        let pad = match version {
            Version::V2 => {
                let sequence = (monotonic().max(0) as u64 / 1_000_000_000) as u32;
                base[14..16].copy_from_slice(&2u16.to_be_bytes());
                base[16..18].fill(0);
                base[18] = u8::try_from(self.send_id).map_err(|_| native_abi::ERR_BAD_STATE)?;
                base[19] = DIGEST_LEN as u8;
                base[20..24].copy_from_slice(&sequence.to_be_bytes());
                mac.update(base);
                APAD
            }
            Version::V3 => {
                let IpAddr::V6(source) = source else {
                    return Err(native_abi::ERR_BAD_STATE);
                };
                // Hello/DBD 通过 AT 位告知邻居所有类型都使用认证尾部。
                match kind {
                    Kind::Hello => base[16 + 6] |= (AT >> 8) as u8,
                    Kind::DatabaseDescription => base[16 + 2] |= (AT >> 8) as u8,
                    _ => {}
                }
                tail[..2].copy_from_slice(&1u16.to_be_bytes());
                tail[2..4].copy_from_slice(&((TRAILER_HEADER + DIGEST_LEN) as u16).to_be_bytes());
                tail[4..6].fill(0);
                tail[6..8].copy_from_slice(&self.send_id.to_be_bytes());
                tail[8..16].copy_from_slice(&sequence.next_v3()?.to_be_bytes());
                mac.update(base);
                mac.update(&tail[..TRAILER_HEADER]);
                let mut pad = APAD;
                pad[..16].copy_from_slice(&source.octets());
                pad
            }
        };
        mac.update(&pad);
        let offset = tail.len() - DIGEST_LEN;
        tail[offset..].copy_from_slice(&mac.finalize().into_bytes());
        Ok(())
    }

    pub(super) fn reset_v2(&mut self) {
        self.v2_received = None;
    }

    pub(super) fn verify(&mut self, packet: Packet<'_>, source: IpAddr) -> Result<(), Failure> {
        match packet.header().fields {
            HeaderFields::V2 {
                authentication_type: 2,
                authentication,
            } => {
                let tail = packet.trailer();
                if usize::from(authentication[3]) != DIGEST_LEN {
                    return Err(Failure::Malformed);
                }
                let (digest, lls) = tail.split_at_checked(DIGEST_LEN).ok_or(Failure::Missing)?;
                if packet.lls_len(lls).map_err(|_| Failure::Malformed)? != lls.len() {
                    return Err(Failure::Malformed);
                }
                let sequence =
                    u32::from_be_bytes(authentication[4..].try_into().expect("v2 sequence field"));
                let mut mac = self.mac(u16::from(authentication[2]))?;
                mac.update(packet.bytes());
                mac.update(&APAD);
                mac.verify_slice(digest)
                    .map_err(|_| Failure::InvalidDigest)?;
                if self.v2_received.is_some_and(|previous| sequence < previous) {
                    return Err(Failure::Replay);
                }
                self.v2_received = Some(sequence);
            }
            HeaderFields::V2 { .. } => return Err(Failure::Missing),
            HeaderFields::V3 { .. } => {
                if packet.options().is_some_and(|options| options & AT == 0) {
                    return Err(Failure::Missing);
                }
                let tail = packet.trailer();
                let lls_length = packet.lls_len(tail).map_err(|_| Failure::Malformed)?;
                let (lls, trailer) = tail.split_at(lls_length);
                let (header, digest) = trailer
                    .split_at_checked(TRAILER_HEADER)
                    .ok_or(Failure::Missing)?;
                if digest.len() != DIGEST_LEN
                    || header[..2] != 1u16.to_be_bytes()
                    || header[2..4] != ((TRAILER_HEADER + DIGEST_LEN) as u16).to_be_bytes()
                {
                    return Err(Failure::Malformed);
                }
                let IpAddr::V6(source) = source else {
                    return Err(Failure::Malformed);
                };
                let key = u16::from_be_bytes([header[6], header[7]]);
                let sequence =
                    u64::from_be_bytes(header[8..16].try_into().expect("v3 sequence field"));
                let mut mac = self.mac(key)?;
                let mut pad = APAD;
                pad[..16].copy_from_slice(&source.octets());
                mac.update(packet.bytes());
                mac.update(lls);
                mac.update(header);
                mac.update(&pad);
                mac.verify_slice(digest)
                    .map_err(|_| Failure::InvalidDigest)?;
                let router = packet.header().router_id;
                let index = usize::from(packet.bytes()[1] - 1);
                if let Some(replay) = self.v3_received.get_mut(&router) {
                    if replay.accepted[index].is_some_and(|previous| sequence <= previous) {
                        return Err(Failure::Replay);
                    }
                    replay.accepted[index] = Some(sequence);
                } else {
                    let mut replay = Replay::default();
                    replay.accepted[index] = Some(sequence);
                    // 资源耗尽时拒绝认证，不能接受无法记录重放高水位的报文。
                    self.v3_received
                        .try_insert(router, replay)
                        .map_err(|_| Failure::Malformed)?;
                }
            }
        }
        Ok(())
    }

    fn mac(&self, id: u16) -> Result<Sha256Mac, Failure> {
        self.keys.get(&id).cloned().ok_or(Failure::UnknownKey)
    }
}

pub(super) fn present(packet: Packet<'_>) -> bool {
    match packet.header().fields {
        HeaderFields::V2 {
            authentication_type,
            ..
        } => authentication_type != 0,
        HeaderFields::V3 { .. } => {
            packet.options().is_some_and(|options| options & AT != 0)
                || packet
                    .lls_len(packet.trailer())
                    .is_ok_and(|length| length < packet.trailer().len())
        }
    }
}
