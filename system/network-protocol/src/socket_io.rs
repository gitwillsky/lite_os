//! 固定 socket 数据记录。地址是值而不是指针；未知位、宽度和保留字在使用前拒绝。
use super::*;
use queue::Record;
mod packet_info;
use packet_info::{CONTROL_BYTES, receive_info};
pub(super) use packet_info::{get_receive_info, payload_length};

pub(super) const SEND: u64 = 1;
pub(super) const RECEIVE: u64 = 2;
pub(super) const POLL: u64 = 3;
pub(super) const RECEIVE_ERROR: u64 = 4;
mod error;
pub(super) use error::get_error;
pub(super) fn request(op: u64, length: usize, peek: bool) -> Record {
    let mut record = [0; queue::WORDS];
    record[1] = op;
    record[2] = length.min(TRANSFER_LIMIT) as u64;
    record[3] = u64::from(peek);
    record
}
fn invalid() -> Status {
    native_abi::ERR_INVALID_ARGS
}
fn narrow(value: u64) -> Result<u32, Status> {
    u32::try_from(value).map_err(|_| invalid())
}
fn ipv4(value: u64) -> Result<Ipv4Addr, Status> {
    Ok(Ipv4Addr::from(narrow(value)?.to_be_bytes()))
}
pub(super) fn inet(record: &mut Record, address: Option<InetAddress>) {
    match address {
        Some(InetAddress::V4(address)) => {
            record[4] = 4;
            record[5] = u64::from(u32::from_be_bytes(address.ip().octets()));
            record[7] = u64::from(address.port());
        }
        Some(InetAddress::V6(address)) => {
            record[4] = 6;
            let bytes = address.ip().octets();
            record[5] = u64::from_be_bytes(bytes[..8].try_into().expect("fixed IPv6 half"));
            record[6] = u64::from_be_bytes(bytes[8..].try_into().expect("fixed IPv6 half"));
            record[7] = u64::from(address.port()) | u64::from(address.scope_id()) << 16;
            record[12] = u64::from(address.flowinfo());
        }
        None => {}
    }
}
pub(super) fn get_inet(record: &Record) -> Result<Option<InetAddress>, Status> {
    match record[4] {
        0 if [record[5], record[6], record[7], record[12]]
            .iter()
            .all(|word| *word == 0) =>
        {
            Ok(None)
        }
        4 if record[6] == 0 && record[12] == 0 => {
            Ok(Some(InetAddress::V4(core::net::SocketAddrV4::new(
                ipv4(record[5])?,
                u16::try_from(record[7]).map_err(|_| invalid())?,
            ))))
        }
        6 if record[7] >> 48 == 0 => {
            let mut bytes = [0; 16];
            bytes[..8].copy_from_slice(&record[5].to_be_bytes());
            bytes[8..].copy_from_slice(&record[6].to_be_bytes());
            Ok(Some(InetAddress::V6(core::net::SocketAddrV6::new(
                bytes.into(),
                record[7] as u16,
                narrow(record[12])?,
                (record[7] >> 16) as u32,
            ))))
        }
        _ => Err(invalid()),
    }
}

pub(super) fn packet(record: &mut Record, address: Option<PacketAddress>) {
    if let Some(address) = address {
        record[4] = 1;
        record[5] = address.protocol as u64 | (address.interface_index as u32 as u64) << 16;
        record[6] = address.hardware_type as u64
            | (address.packet_type as u64) << 16
            | (address.address_length as u64) << 24;
        record[7] = u64::from_le_bytes(address.address);
    }
}
pub(super) fn get_packet(record: &Record) -> Result<Option<PacketAddress>, Status> {
    if record[5] >> 48 != 0 || record[6] >> 32 != 0 || record[12] != 0 {
        return Err(invalid());
    }
    let address = PacketAddress {
        protocol: record[5] as u16,
        interface_index: (record[5] >> 16) as u32 as i32,
        hardware_type: record[6] as u16,
        packet_type: (record[6] >> 16) as u8,
        address_length: (record[6] >> 24) as u8,
        address: record[7].to_le_bytes(),
    };
    match record[4] {
        0 if record[5..8].iter().all(|word| *word == 0) => Ok(None),
        1 => Ok(Some(address)),
        _ => Err(invalid()),
    }
}
pub(super) fn packet_information(record: &mut Record, info: PacketInformation) {
    record[8] = match info.checksum {
        PacketChecksum::Unverified => 0,
        PacketChecksum::Partial => 1,
        PacketChecksum::Verified => 2,
    } | (u64::from(info.network_offset) << 16);
    record[9] = info.timestamp_ns;
}
pub(super) fn get_packet_information(record: &Record) -> Result<PacketInformation, Status> {
    if record[8] & !0xffff_0003 != 0
        || record[8] >> 16 > record[3]
        || record[10..].iter().any(|&word| word != 0)
    {
        return Err(invalid());
    }
    Ok(PacketInformation {
        checksum: match record[8] & 3 {
            0 => PacketChecksum::Unverified,
            1 => PacketChecksum::Partial,
            2 => PacketChecksum::Verified,
            _ => return Err(invalid()),
        },
        network_offset: (record[8] >> 16) as u16,
        timestamp_ns: record[9],
    })
}
fn poll(state: SocketPollState) -> u64 {
    u64::from(state.readable)
        | u64::from(state.writable) << 1
        | u64::from(state.hangup) << 2
        | u64::from(state.read_closed) << 3
        | u64::from(state.error) << 4
}
pub(super) fn get_poll(record: &Record) -> Result<SocketPollState, Status> {
    let bits = record[2];
    if bits & !31 != 0 || record[3..].iter().any(|&word| word != 0) {
        return Err(invalid());
    }
    Ok(SocketPollState {
        readable: bits & 1 != 0,
        writable: bits & 2 != 0,
        hangup: bits & 4 != 0,
        read_closed: bits & 8 != 0,
        error: bits & 16 != 0,
    })
}
/// 成功时只校验数据面通用结果；具体地址在复制/解释之前由对应解码器校验。
pub(super) fn result(record: &Record) -> Result<(), SocketError> {
    match record[1] {
        0 => Ok(()),
        code => Err(u8::try_from(code)
            .ok()
            .and_then(SocketError::from_code)
            .unwrap_or(SocketError::NetworkDown)),
    }
}
/// 校验各操作真正使用的信封字段；保留字段不能在不同操作间偷带另一个地址或元数据。
fn validate_request(request: &Record) -> Result<(), Status> {
    if request[0] == 0
        || request[2] > TRANSFER_LIMIT as u64
        || request[3] > 1
        || request[13..].iter().any(|&v| v != 0)
    {
        return Err(invalid());
    }
    match request[1] {
        SEND if request[3] == 0 && request[9..12] == [0; 3] => {
            packet_info::payload_length(
                request[2] as usize,
                usize::try_from(request[8]).map_err(|_| invalid())?,
            )
            .map_err(|_| invalid())?;
            Ok(())
        }
        RECEIVE if request[4..13].iter().all(|word| *word == 0) => Ok(()),
        RECEIVE_ERROR if request[3..13].iter().all(|word| *word == 0) => Ok(()),
        POLL if request[2..13].iter().all(|word| *word == 0) => Ok(()),
        _ => Err(invalid()),
    }
}

pub(super) fn write_send_payload(
    ring: &queue::Queue,
    ticket: u64,
    input: &[u8],
    controls: impl ExactSizeIterator<Item = InternetSendPacketInfo>,
) -> Result<(), Status> {
    let count = controls.len();
    packet_info::payload_length(input.len(), count)?;
    let mut written = 0;
    for control in controls {
        if written == count {
            return Err(invalid());
        }
        ring.write_payload_at(
            ticket,
            written * CONTROL_BYTES,
            &packet_info::encode(control),
        )?;
        written += 1;
    }
    if written != count {
        return Err(invalid());
    }
    ring.write_payload_at(ticket, count * CONTROL_BYTES, input)
}

/// 调用协议引擎，保持短写、截断、PEEK、packet info 与即时错误的原有语义。
pub(super) fn execute(
    socket: &ServedSocket,
    ring: &queue::Queue,
    request: Record,
    scratch: &mut Vec<u8>,
    controls: &mut Vec<InternetSendPacketInfo>,
) -> Result<Record, Status> {
    validate_request(&request)?;
    let mut response = [0; queue::WORDS];
    response[0] = request[0];
    let length = request[2] as usize;
    let outcome = match request[1] {
        SEND => {
            scratch.clear();
            let count = usize::try_from(request[8]).map_err(|_| invalid())?;
            let total = packet_info::payload_length(length, count)?;
            if let Err(status) = ring.read_payload(request[0], total, scratch) {
                if status != native_abi::ERR_NO_MEMORY {
                    return Err(status);
                }
                response[1] = SocketError::NoMemory.code() as u64;
                return Ok(response);
            }
            controls.clear();
            if controls.try_reserve_exact(count).is_err() {
                response[1] = SocketError::NoMemory.code() as u64;
                return Ok(response);
            }
            let prefix = count * CONTROL_BYTES;
            for encoded in scratch[..prefix].as_chunks::<CONTROL_BYTES>().0 {
                controls.push(packet_info::decode(encoded)?);
            }
            match socket {
                ServedSocket::Inet(socket) => {
                    socket.send_to(&scratch[prefix..], get_inet(&request)?, controls)
                }
                ServedSocket::Packet(socket) => {
                    if request[8..13].iter().any(|word| *word != 0) {
                        return Err(invalid());
                    }
                    socket.send_to(&scratch[prefix..], get_packet(&request)?)
                }
            }
            .map(|count| {
                response[2] = count as u64;
            })
        }
        RECEIVE | RECEIVE_ERROR => match (request[1], socket) {
            (RECEIVE_ERROR, ServedSocket::Inet(socket)) => {
                socket.receive_error(length).map(|received| {
                    error::put_error(&mut response, received.error);
                    (received.data, received.full_length)
                })
            }
            (RECEIVE_ERROR, ServedSocket::Packet(_)) => Err(SocketError::OperationNotSupported),
            (_, ServedSocket::Inet(socket)) => {
                socket.receive(length, request[3] != 0).map(|received| {
                    inet(&mut response, received.source);
                    receive_info(&mut response, received.packet_info);
                    (received.data, received.full_length)
                })
            }
            (_, ServedSocket::Packet(socket)) => {
                socket.receive(length, request[3] != 0).map(|received| {
                    packet(&mut response, Some(received.source));
                    packet_information(&mut response, received.information);
                    (received.data, received.full_length)
                })
            }
        }
        .and_then(|(data, full_length)| {
            if data.len() > length || full_length < data.len() {
                return Err(SocketError::Invalid);
            }
            ring.write_payload(request[0], &data)
                .map_err(client::transport)?;
            response[2] = data.len() as u64;
            response[3] = full_length as u64;
            Ok(())
        }),
        POLL => {
            response[2] = poll(match socket {
                ServedSocket::Inet(socket) => socket.poll_state(),
                ServedSocket::Packet(socket) => socket.poll_state(),
            });
            Ok(())
        }
        _ => return Err(invalid()),
    };
    if let Err(error) = outcome {
        response = [0; queue::WORDS];
        response[0] = request[0];
        response[1] = error.code() as u64;
    }
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn queue_endpoint_preserves_ipv6_fields_and_rejects_noncanonical_or_cross_family_bits() {
        let ipv4 = InetAddress::V4(core::net::SocketAddrV4::new(
            Ipv4Addr::new(192, 0, 2, 1),
            0x1234,
        ));
        let ipv6 = InetAddress::V6(core::net::SocketAddrV6::new(
            "fe80::abcd".parse().unwrap(),
            0xface,
            0x1234_5678,
            0xdead_beef,
        ));
        for address in [None, Some(ipv4), Some(ipv6)] {
            let mut record = request(SEND, 8, false);
            inet(&mut record, address);
            assert_eq!(get_inet(&record), Ok(address));
            match address {
                Some(InetAddress::V4(_)) => assert_eq!(&record[4..8], &[4, 0xc000_0201, 0, 0x1234]),
                Some(InetAddress::V6(_)) => {
                    assert_eq!(
                        &record[4..8],
                        &[6, 0xfe80_0000_0000_0000, 0xabcd, 0xdead_beef_face]
                    );
                    assert_eq!(record[12], 0x1234_5678);
                }
                None => {}
            }
        }
        for (value, slot, bits) in [
            (Some(ipv4), 5, 1u64 << 32),
            (Some(ipv4), 6, 1),
            (Some(ipv4), 7, 1 << 16),
            (Some(ipv4), 12, 1),
            (Some(ipv6), 7, 1 << 48),
            (Some(ipv6), 12, 1 << 32),
            (None, 5, 1),
            (None, 12, 1),
        ] {
            let mut record = request(SEND, 0, false);
            inet(&mut record, value);
            record[slot] |= bits;
            assert!(get_inet(&record).is_err());
        }
    }
    #[test]
    fn envelope_and_absent_packet_info_cannot_smuggle_another_operation_payload() {
        for op in [SEND, RECEIVE, POLL, RECEIVE_ERROR] {
            let mut record = request(op, 0, false);
            record[0] = 17;
            assert!(validate_request(&record).is_ok());
            let mut invalid_id = record;
            invalid_id[0] = 0;
            assert!(validate_request(&invalid_id).is_err());
            record[13] = 1;
            assert!(validate_request(&record).is_err());
            record[13] = 0;
            if op != SEND {
                record[4] = 6;
                assert!(validate_request(&record).is_err());
            }
        }
        let mut record = request(SEND, 0, true);
        record[0] = 17;
        assert!(validate_request(&record).is_err());
        for slot in [9, 10, 11] {
            let mut record = [0; queue::WORDS];
            record[slot] = 1;
            record[0] = 17;
            record[1] = SEND;
            assert!(validate_request(&record).is_err());
        }
        let mut absent_packet = [0; queue::WORDS];
        absent_packet[7] = 1;
        assert!(get_packet(&absent_packet).is_err());
        let mut packet = [0; queue::WORDS];
        packet[12] = 1;
        assert!(get_packet(&packet).is_err());
    }
}
