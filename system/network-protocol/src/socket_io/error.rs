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
}

pub(crate) fn get_error(record: &Record) -> Result<InternetError, Status> {
    if record[8] >> 32 != 0 || record[13..].iter().any(|&word| word != 0) {
        return Err(invalid());
    }
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
    Ok(InternetError {
        error,
        origin,
        kind,
        code,
        info: record[9] as u32,
        destination,
        offender,
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
            destination: "[fe80::10%4]:9090".parse().unwrap(),
            offender: Some("[fe80::1%4]:0".parse().unwrap()),
        };
        let mut record = [0; queue::WORDS];
        put_error(&mut record, error);
        assert_eq!(get_error(&record), Ok(error));
        record[8] ^= 1 << 8;
        assert_eq!(get_error(&record), Err(invalid()));
    }
}
