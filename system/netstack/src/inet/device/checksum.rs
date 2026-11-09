//! 仅处理实际协议 emitter 明确请求的 TX checksum；RAW 不进入本路径。
use device_model::network::{NetworkChecksum, NetworkError};
use packet_wire::checksum::InternetChecksum;

pub(super) fn prepare(frame: &mut [u8]) -> Result<Option<NetworkChecksum>, NetworkError> {
    let ethernet_type = packet_wire::ethernet::EthernetFrame::parse(frame)
        .map_err(|_| NetworkError::InvalidConfiguration)?
        .type_or_length();
    let mut seed = InternetChecksum::new();
    let (start, protocol, length) = match ethernet_type {
        0x0800 => {
            if frame.len() < 34 || frame[14] >> 4 != 4 {
                return Err(NetworkError::InvalidConfiguration);
            }
            let header = usize::from(frame[14] & 15) * 4;
            let total = usize::from(u16::from_be_bytes([frame[16], frame[17]]));
            if header < 20
                || total < header
                || total + 14 != frame.len()
                || u16::from_be_bytes([frame[20], frame[21]]) & 0x3fff != 0
            {
                return Err(NetworkError::InvalidConfiguration);
            }
            seed.add(&frame[26..34]);
            (14 + header, frame[23], total - header)
        }
        0x86dd => {
            if frame.len() < 54 || frame[14] >> 4 != 6 {
                return Err(NetworkError::InvalidConfiguration);
            }
            let length = usize::from(u16::from_be_bytes([frame[18], frame[19]]));
            if length + 54 != frame.len() {
                return Err(NetworkError::InvalidConfiguration);
            }
            // 此 adapter 的协议 emitter 产生无扩展头的 TCP/UDP；RAW 扩展头始终保持原字节。
            seed.add(&frame[22..54]);
            (54, frame[20], length)
        }
        0x0806 => return Ok(None), // 邻居解析可借用原 IP token 发送 ARP，不能把它当作 transport。
        _ => return Err(NetworkError::InvalidConfiguration),
    };
    let (offset, header) = match protocol {
        6 => {
            let header = packet_wire::tcp::TcpPrefix::parse(&frame[start..start + length])
                .map_err(|_| NetworkError::InvalidConfiguration)?;
            (16, header.bytes().len())
        }
        17 => {
            if length < 8
                || usize::from(u16::from_be_bytes([frame[start + 4], frame[start + 5]])) != length
            {
                return Err(NetworkError::InvalidConfiguration);
            }
            (6, 8)
        }
        _ => return Err(NetworkError::InvalidConfiguration),
    };
    seed.add(&[0, protocol]);
    seed.add(&(length as u16).to_be_bytes());
    frame[start + offset..start + offset + 2].copy_from_slice(&seed.sum().to_be_bytes());
    let checksum = NetworkChecksum {
        start: start as u16,
        offset: offset as u16,
        header_length: (start + header) as u16,
    };
    checksum.validate(frame.len())?;
    Ok(Some(checksum))
}
