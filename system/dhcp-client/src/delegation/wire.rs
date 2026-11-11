//! DHCPv6-PD 收发信封和 IA_PD 的协议准入，选项布局复用 packet-wire。

use alloc::vec::Vec;
use core::net::Ipv6Addr;

use hmac::{Hmac, Mac};
use md5::Md5;
use packet_wire::PacketError;
use packet_wire::dhcpv6::{
    Dhcpv6Header, Dhcpv6Packet, MessageType,
    options::{Duid, OptionValue, Options, OptionsBuilder},
};
use router_protocol::delegation::MAX_PREFIXES;
use zeroize::Zeroizing;

/// IPv6 最小链路 MTU 减去 IPv6 和 UDP 固定头。
pub(super) const OUTPUT_BYTES: usize = 1232;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Prefix {
    pub(super) address: Ipv6Addr,
    pub(super) length: u8,
}

#[derive(Clone, Copy)]
pub(super) struct Lifetime {
    pub(super) prefix: Prefix,
    pub(super) preferred: u32,
    pub(super) valid: u32,
}

#[derive(Clone, Copy)]
pub(super) struct Exchange<'a> {
    pub(super) message: MessageType,
    pub(super) transaction: u32,
    pub(super) client: &'a [u8],
    pub(super) server: Option<&'a [u8]>,
    pub(super) iaid: u32,
    pub(super) accept_reconfigure: bool,
}

pub(super) struct Response<'a> {
    pub(super) server: &'a [u8],
    pub(super) preference: u8,
    pub(super) solicit_maximum: Option<u32>,
    pub(super) association: Option<Association>,
    pub(super) status: u16,
    pub(super) information_maximum: Option<u32>,
    pub(super) information_refresh: Option<u32>,
    pub(super) reconfigure_key: Option<ReconfigureKey<'a>>,
}

pub(super) struct Association {
    pub(super) status: u16,
    pub(super) renew: u32,
    pub(super) rebind: u32,
    pub(super) prefixes: Vec<Lifetime>,
}

fn status_code(options: Options<'_>) -> Result<u16, PacketError> {
    match options
        .single(13)?
        .map(|option| option.decode())
        .transpose()?
    {
        Some(OptionValue::Status { code, .. }) => Ok(code),
        None => Ok(0),
        _ => Err(PacketError::Malformed),
    }
}

fn server_for<'a>(
    packet: Dhcpv6Packet<'a>,
    client_id: &[u8],
    server_id: Option<&[u8]>,
) -> Result<&'a [u8], PacketError> {
    let Some(OptionValue::ClientIdentifier(client)) = packet
        .single_option(1)?
        .map(|option| option.decode())
        .transpose()?
    else {
        return Err(PacketError::Malformed);
    };
    let Some(OptionValue::ServerIdentifier(server)) = packet
        .single_option(2)?
        .map(|option| option.decode())
        .transpose()?
    else {
        return Err(PacketError::Malformed);
    };
    if client.bytes() != client_id || server_id.is_some_and(|id| id != server.bytes()) {
        return Err(PacketError::Malformed);
    }
    Ok(server.bytes())
}

struct Authentication<'a> {
    kind: u8,
    replay: u64,
    value: &'a [u8; 16],
    value_offset: usize,
}

impl<'a> Authentication<'a> {
    fn decode(packet: Dhcpv6Packet<'a>) -> Result<Option<Self>, PacketError> {
        let Some(option) = packet.single_option(11)? else {
            return Ok(None);
        };
        let OptionValue::Authentication {
            protocol: 3,
            algorithm: 1,
            replay_method: 0,
            replay,
            information,
        } = option.decode()?
        else {
            return Err(PacketError::Malformed);
        };
        let (&kind, value) = information.split_first().ok_or(PacketError::Malformed)?;
        Ok(Some(Self {
            kind,
            replay,
            value: value.try_into().map_err(|_| PacketError::Malformed)?,
            // 普通 DHCPv6 头、TLV 头、认证公共字段和信息类型，共 20 字节。
            value_offset: option.offset() + 4 + 4 + 11 + 1,
        }))
    }
}

pub(super) struct ReconfigureKey<'a> {
    replay: u64,
    key: &'a [u8; 16],
}

pub(super) struct ReconfigureAuth {
    key: Zeroizing<[u8; 16]>,
    replay: u64,
    // 合法的连续触发也须限速，避免快速 Reply 与 Reconfigure 循环。
    accept_after: i64,
}

impl ReconfigureAuth {
    pub(super) fn from_reply(reply: ReconfigureKey<'_>) -> Self {
        Self {
            key: Zeroizing::new(*reply.key),
            replay: reply.replay,
            accept_after: 0,
        }
    }

    pub(super) fn update(&mut self, reply: ReconfigureKey<'_>) {
        if reply.replay > self.replay {
            self.key = Zeroizing::new(*reply.key);
            self.replay = reply.replay;
        }
    }

    pub(super) fn accept(
        &mut self,
        bytes: &[u8],
        client: &[u8],
        server: &[u8],
        now: i64,
    ) -> Result<MessageType, PacketError> {
        let packet = Dhcpv6Packet::parse(bytes)?;
        if packet.message_type() != MessageType::Reconfigure as u8 || now < self.accept_after {
            return Err(PacketError::Malformed);
        }
        server_for(packet, client, Some(server))?;
        let Some(OptionValue::ReconfigureMessage(request)) = packet
            .single_option(19)?
            .map(|option| option.decode())
            .transpose()?
        else {
            return Err(PacketError::Malformed);
        };
        let kind = MessageType::try_from(request)?;
        let Some(auth) = Authentication::decode(packet)? else {
            return Err(PacketError::Malformed);
        };
        if auth.kind != 2 || auth.replay <= self.replay {
            return Err(PacketError::Malformed);
        }
        let mut mac = Hmac::<Md5>::new_from_slice(self.key.as_ref())
            .expect("HMAC accepts the fixed key width");
        let (before, tail) = bytes.split_at(auth.value_offset);
        mac.update(before);
        mac.update(&[0; 16]);
        mac.update(&tail[16..]);
        mac.verify_slice(auth.value)
            .map_err(|_| PacketError::Malformed)?;
        self.replay = auth.replay;
        self.accept_after = now.saturating_add(1_000_000_000);
        Ok(kind)
    }
}

impl Exchange<'_> {
    pub(super) fn encode(
        self,
        elapsed: u16,
        prefixes: impl IntoIterator<Item = Lifetime>,
        output: &mut [u8; OUTPUT_BYTES],
    ) -> Result<usize, PacketError> {
        let mut nested = [0; 29 * MAX_PREFIXES];
        let mut association = OptionsBuilder::new(&mut nested);
        for lease in prefixes {
            association.push(OptionValue::Prefix {
                prefix: lease.prefix.address,
                length: lease.prefix.length,
                preferred_lifetime: lease.preferred,
                valid_lifetime: lease.valid,
                options: Options::parse(&[])?,
            })?;
        }
        let mut options = OptionsBuilder::new(&mut output[4..]);
        options.push(OptionValue::ClientIdentifier(Duid::parse(self.client)?))?;
        if let Some(server) = self.server {
            options.push(OptionValue::ServerIdentifier(Duid::parse(server)?))?;
        }
        options.push(OptionValue::ElapsedTime(elapsed))?;
        if self.accept_reconfigure
            && matches!(self.message, MessageType::Solicit | MessageType::Request)
        {
            options.push(OptionValue::ReconfigureAccept)?;
        }
        if self.message == MessageType::InformationRequest {
            options.push(OptionValue::RequestedOptions(&[
                83u16.to_be_bytes(),
                32u16.to_be_bytes(),
            ]))?;
        } else {
            if matches!(self.message, MessageType::Solicit | MessageType::Request) {
                options.push(OptionValue::RequestedOptions(&[82u16.to_be_bytes()]))?;
            }
            options.push(OptionValue::IdentityAssociation {
                delegation: true,
                identifier: self.iaid,
                renew: 0,
                rebind: 0,
                options: Options::parse(association.finish())?,
            })?;
        }
        let length = 4 + options.finish().len();
        Dhcpv6Header::Client {
            message_type: self.message as u8,
            transaction_id: self.transaction,
        }
        .emit(&mut output[..length])?;
        Ok(length)
    }

    pub(super) fn decode<'a>(self, bytes: &'a [u8]) -> Result<Response<'a>, PacketError> {
        let packet = Dhcpv6Packet::parse(bytes)?;
        let expected = match self.message {
            MessageType::Solicit => MessageType::Advertise,
            _ => MessageType::Reply,
        };
        if packet.header()
            != (Dhcpv6Header::Client {
                message_type: expected as u8,
                transaction_id: self.transaction,
            })
        {
            return Err(PacketError::Malformed);
        }
        let server = server_for(packet, self.client, self.server)?;
        let preference = match packet
            .single_option(7)?
            .map(|option| option.decode())
            .transpose()?
        {
            Some(OptionValue::Preference(value)) => value,
            _ => 0,
        };
        let solicit_maximum = match packet
            .single_option(82)?
            .map(|option| option.decode())
            .transpose()?
        {
            Some(OptionValue::SolicitMaximumTime(value @ 60..=86400)) => Some(value),
            _ => None,
        };
        let options = packet.options();
        let status = status_code(options)?;
        let information = self.message == MessageType::InformationRequest;
        let information_maximum = match packet
            .single_option(83)?
            .map(|option| option.decode())
            .transpose()?
        {
            Some(OptionValue::InformationMaximumTime(value @ 60..=86400)) => Some(value),
            _ => None,
        };
        let information_refresh = if information {
            match packet
                .single_option(32)?
                .map(|option| option.decode())
                .transpose()?
            {
                Some(OptionValue::InformationRefreshTime(value)) => Some(value.max(600)),
                _ => None,
            }
        } else {
            None
        };
        let reconfigure_key =
            if self.accept_reconfigure && status == 0 && self.message == MessageType::Request {
                Authentication::decode(packet)?
                    .filter(|auth| auth.kind == 1)
                    .map(|auth| ReconfigureKey {
                        replay: auth.replay,
                        key: auth.value,
                    })
            } else {
                None
            };
        let mut association = None;
        if status == 0 && !information {
            for option in options.filter(|option| option.code() == 25) {
                let OptionValue::IdentityAssociation {
                    identifier,
                    renew,
                    rebind,
                    options,
                    ..
                } = option.decode()?
                else {
                    unreachable!("IA_PD code")
                };
                if identifier != self.iaid {
                    continue;
                }
                if association.is_some() || (rebind != 0 && renew > rebind) {
                    return Err(PacketError::Malformed);
                }
                association = Some(Association::decode(renew, rebind, options)?);
            }
        }
        Ok(Response {
            server,
            preference,
            solicit_maximum,
            association,
            status,
            information_maximum,
            information_refresh,
            reconfigure_key,
        })
    }
}

impl Association {
    fn decode(renew: u32, rebind: u32, options: Options<'_>) -> Result<Self, PacketError> {
        let status = status_code(options)?;
        let mut prefixes = Vec::new();
        if status == 0 {
            for option in options.filter(|option| option.code() == 26) {
                let Ok(OptionValue::Prefix {
                    preferred_lifetime,
                    valid_lifetime,
                    length,
                    prefix,
                    options,
                }) = option.decode()
                else {
                    continue;
                };
                let host_mask = u128::MAX.checked_shr(u32::from(length)).unwrap_or(0);
                if status_code(options)? != 0
                    || u128::from(prefix) & host_mask != 0
                    || preferred_lifetime > valid_lifetime
                    || prefix.is_multicast()
                    || prefix.is_unicast_link_local()
                {
                    continue;
                }
                if prefixes.len() == MAX_PREFIXES {
                    return Err(PacketError::Capacity);
                }
                prefixes.try_reserve(1).map_err(|_| PacketError::Capacity)?;
                prefixes.push(Lifetime {
                    prefix: Prefix {
                        address: prefix,
                        length,
                    },
                    preferred: preferred_lifetime,
                    valid: valid_lifetime,
                });
            }
            prefixes.sort_unstable_by_key(|lease| lease.prefix);
            if prefixes
                .windows(2)
                .any(|pair| pair[0].prefix == pair[1].prefix)
            {
                return Err(PacketError::Malformed);
            }
        }
        Ok(Self {
            status,
            renew,
            rebind,
            prefixes,
        })
    }
}
