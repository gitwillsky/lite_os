//! RECEIVE_ERROR 专用记录。只在这个操作解释原目的、offender 和错误字段。
use super::*;

pub(super) fn put_error(record: &mut Record, error: InternetError) {
    inet(record, Some(error.destination));
    let origin = match error.origin {
        InternetErrorOrigin::Local => 1,
        InternetErrorOrigin::Icmpv4 => 2,
        InternetErrorOrigin::Icmpv6 => 3,
    };
    record[8] = u64::from(error.error.code())
        | origin << 8
        | u64::from(error.kind) << 16
        | u64::from(error.code) << 24;
    if let Some(extension) = error.extension {
        record[8] |=
            u64::from(extension.offset) << 32 | 1 << 48 | u64::from(extension.invalid) << 49;
    }
    record[9] = u64::from(error.info);
    match error.offender {
        Some(InetAddress::V4(address)) => {
            record[10] = u64::from(u32::from_be_bytes(address.ip().octets()));
        }
        Some(InetAddress::V6(address)) => {
            let bytes = address.ip().octets();
            record[10] = u64::from_be_bytes(bytes[..8].try_into().expect("IPv6 prefix"));
            record[11] = u64::from_be_bytes(bytes[8..].try_into().expect("IPv6 suffix"));
            record[9] |= u64::from(address.scope_id()) << 32;
        }
        None => {}
    }
    let [tag, interface, address_high, address_low] =
        packet_info::encode_receive_info(error.packet_info);
    // word 12 仍归原目的 flowinfo。错误操作用同一 word 存放 tag 与接口。
    record[13] = tag | interface << 32;
    record[14] = address_high;
    record[15] = address_low;
}

pub(crate) fn get_error(record: &Record) -> Result<InternetError, Status> {
    let destination = get_inet(record)?.ok_or_else(invalid)?;
    let error = SocketError::from_code(record[8] as u8).ok_or_else(invalid)?;
    let kind = (record[8] >> 16) as u8;
    let code = (record[8] >> 24) as u8;
    let is_ipv4 = destination.ip().to_canonical().is_ipv4();
    let origin = match (record[8] >> 8) as u8 {
        1 if kind == 0 && code == 0 => InternetErrorOrigin::Local,
        2 if is_ipv4 => InternetErrorOrigin::Icmpv4,
        3 if !is_ipv4 => InternetErrorOrigin::Icmpv6,
        _ => return Err(invalid()),
    };
    let scope = (record[9] >> 32) as u32;
    let offender = match (origin, destination) {
        (InternetErrorOrigin::Local, _) if record[10..12] == [0, 0] && scope == 0 => None,
        (InternetErrorOrigin::Local, _) => return Err(invalid()),
        (_, InetAddress::V4(_)) if record[11] == 0 && scope == 0 => {
            Some(InetAddress::new(ipv4(record[10])?.into(), 0))
        }
        (_, InetAddress::V6(_)) => {
            let mut address = [0; 16];
            address[..8].copy_from_slice(&record[10].to_be_bytes());
            address[8..].copy_from_slice(&record[11].to_be_bytes());
            Some(InetAddress::V6(core::net::SocketAddrV6::new(
                address.into(),
                0,
                0,
                scope,
            )))
        }
        _ => return Err(invalid()),
    };
    if offender.is_some_and(|address| address.ip().to_canonical().is_ipv4() != is_ipv4) {
        return Err(invalid());
    }
    let offset = (record[8] >> 32) as u16;
    let extension = match record[8] >> 48 {
        0 if offset == 0 => None,
        flags @ (1 | 3)
            if origin != InternetErrorOrigin::Local && u64::from(offset) + 4 <= record[3] =>
        {
            Some(IcmpErrorExtension {
                offset,
                invalid: flags == 3,
            })
        }
        _ => return Err(invalid()),
    };
    let packet_info = packet_info::decode_receive_info(&[
        u64::from(record[13] as u32),
        record[13] >> 32,
        record[14],
        record[15],
    ])?;
    if packet_info.is_some_and(|info| {
        origin == InternetErrorOrigin::Local
            || match info {
                InternetPacketInfo::Ipv4 { .. } | InternetPacketInfo::DualStack { .. } => !is_ipv4,
                InternetPacketInfo::Ipv6 { destination, .. } => {
                    destination.to_ipv4_mapped().is_some() != is_ipv4
                }
            }
    }) {
        return Err(invalid());
    }
    Ok(InternetError {
        error,
        origin,
        kind,
        code,
        info: record[9] as u32,
        destination,
        offender,
        extension,
        packet_info,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // The error record reuses address slots from normal receive with a different
    // ancillary layout. Check both a scoped IPv6 error and a malformed origin.
    #[test]
    fn receive_error_preserves_destination_offender_and_scope() {
        let error = InternetError {
            error: SocketError::MessageTooLarge,
            origin: InternetErrorOrigin::Icmpv6,
            kind: 2,
            code: 0,
            info: 1280,
            destination: InetAddress::V6(core::net::SocketAddrV6::new(
                "fe80::10".parse().unwrap(),
                9090,
                0x12345,
                4,
            )),
            offender: Some("[fe80::1%4]:0".parse().unwrap()),
            extension: Some(IcmpErrorExtension {
                offset: 80,
                invalid: true,
            }),
            packet_info: Some(InternetPacketInfo::Ipv6 {
                interface_index: 4,
                destination: "fe80::20".parse().unwrap(),
            }),
        };
        let mut record = [0; queue::WORDS];
        record[3] = 96;
        put_error(&mut record, error);
        assert_eq!(get_error(&record), Ok(error));
        record[3] = 83;
        assert_eq!(get_error(&record), Err(invalid()));
        record[3] = 96;
        record[8] ^= 1 << 8;
        assert_eq!(get_error(&record), Err(invalid()));
    }
}
