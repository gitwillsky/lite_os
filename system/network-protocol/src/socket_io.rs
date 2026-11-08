//! 固定 socket 数据记录。地址是值而不是指针；未知位、宽度和保留字在使用前拒绝。
use super::*;
use queue::Record;

pub(super) const SEND: u64 = 1;
pub(super) const RECEIVE: u64 = 2;
pub(super) const POLL: u64 = 3;
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
    if let Some(address) = address {
        record[4] = 1;
        record[5] = u32::from_be_bytes(address.address.octets()) as u64;
        record[6] = address.port as u64;
    }
}
pub(super) fn get_inet(record: &Record) -> Result<Option<InetAddress>, Status> {
    let address = InetAddress {
        address: ipv4(record[5])?,
        port: u16::try_from(record[6]).map_err(|_| invalid())?,
    };
    match record[4] {
        0 => Ok(None),
        1 => Ok(Some(address)),
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
    if record[5] >> 48 != 0 || record[6] >> 32 != 0 {
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
        0 => Ok(None),
        1 => Ok(Some(address)),
        _ => Err(invalid()),
    }
}
pub(super) fn send_info(record: &mut Record, info: Option<InternetSendPacketInfo>) {
    if let Some(info) = info {
        record[8] = 1
            | (u64::from(info.interface_index.is_some()) << 1)
            | (u64::from(info.source_address.is_some()) << 2);
        record[9] = info.interface_index.unwrap_or(0) as u64;
        record[10] = u32::from_be_bytes(
            info.source_address
                .unwrap_or(Ipv4Addr::UNSPECIFIED)
                .octets(),
        ) as u64;
    }
}
fn get_send_info(record: &Record) -> Result<Option<InternetSendPacketInfo>, Status> {
    if record[8] & !7 != 0 || record[8] != 0 && record[8] & 1 == 0 {
        return Err(invalid());
    }
    let info = InternetSendPacketInfo {
        interface_index: (record[8] & 2 != 0).then_some(narrow(record[9])?),
        source_address: (record[8] & 4 != 0).then_some(ipv4(record[10])?),
    };
    Ok((record[8] & 1 != 0).then_some(info))
}
fn receive_info(record: &mut Record, info: Option<InternetPacketInfo>) {
    if let Some(info) = info {
        record[8] = 1;
        record[9] = info.interface_index as u64;
        record[10] = u32::from_be_bytes(info.specific_address.octets()) as u64;
        record[11] = u32::from_be_bytes(info.destination.octets()) as u64;
    }
}
pub(super) fn get_receive_info(record: &Record) -> Result<Option<InternetPacketInfo>, Status> {
    let info = InternetPacketInfo {
        interface_index: narrow(record[9])?,
        specific_address: ipv4(record[10])?,
        destination: ipv4(record[11])?,
    };
    match record[8] {
        0 => Ok(None),
        1 => Ok(Some(info)),
        _ => Err(invalid()),
    }
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
    if bits & !31 != 0 {
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
/// 调用协议引擎，保持短写、截断、PEEK、packet info 与即时错误的原有语义。
pub(super) fn execute(
    socket: &ServedSocket,
    ring: &queue::Queue,
    request: Record,
    scratch: &mut Vec<u8>,
) -> Result<Record, Status> {
    if request[0] == 0
        || request[2] > TRANSFER_LIMIT as u64
        || request[3] > 1
        || request[12..].iter().any(|&v| v != 0)
    {
        return Err(invalid());
    }
    let mut response = [0; queue::WORDS];
    response[0] = request[0];
    let length = request[2] as usize;
    let outcome = match request[1] {
        SEND => {
            scratch.clear();
            if let Err(status) = ring.read_payload(request[0], length, scratch) {
                if status != native_abi::ERR_NO_MEMORY {
                    return Err(status);
                }
                response[1] = SocketError::NoMemory.code() as u64;
                return Ok(response);
            }
            match socket {
                ServedSocket::Inet(socket) => {
                    socket.send_to(scratch, get_inet(&request)?, get_send_info(&request)?)
                }
                ServedSocket::Packet(socket) => socket.send_to(scratch, get_packet(&request)?),
            }
            .map(|count| {
                response[2] = count as u64;
            })
        }
        RECEIVE => match socket {
            ServedSocket::Inet(socket) => socket.receive(length, request[3] != 0).map(|received| {
                inet(&mut response, received.source);
                receive_info(&mut response, received.packet_info);
                (received.data, received.full_length)
            }),
            ServedSocket::Packet(socket) => {
                socket.receive(length, request[3] != 0).map(|received| {
                    packet(&mut response, Some(received.source));
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
