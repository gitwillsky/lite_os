//! 以太网卡 v9：OPEN 显式协商 pair 数；一次授权 VMO，四条共享环传递收发，事件合并通知。
mod client;
mod queue;
mod rss;
mod server;

use crate::{Reader, Writer};
pub use client::{RemoteNetwork, connect};
use device_model::network::{
    NetworkChecksum, NetworkError, NetworkTcpSegmentation, NetworkTcpVersion,
    NetworkTransmitCapabilities,
};
use native_runtime::native_abi::{self, Status};
pub use server::publish;

const NETWORK_VERSION: u16 = 9;
const STATISTICS: u32 = 8;
const LINK: u32 = 9;
const MEMORY: u32 = 12;
const QUEUE: u32 = 13;
const RSS_CONFIG: u32 = 14;

fn encode_error(error: NetworkError) -> Status {
    match error {
        NetworkError::InvalidConfiguration => native_abi::ERR_INVALID_ARGS,
        NetworkError::WouldBlock => native_abi::ERR_SHOULD_WAIT,
        NetworkError::NoMemory => native_abi::ERR_NO_MEMORY,
        NetworkError::FrameTooLarge => native_abi::ERR_OUT_OF_RANGE,
        NetworkError::Device => native_abi::ERR_IO,
        NetworkError::Disconnected => native_abi::ERR_PEER_CLOSED,
    }
}
fn decode_error(status: Status) -> NetworkError {
    match status {
        native_abi::ERR_INVALID_ARGS => NetworkError::InvalidConfiguration,
        native_abi::ERR_SHOULD_WAIT => NetworkError::WouldBlock,
        native_abi::ERR_NO_MEMORY => NetworkError::NoMemory,
        native_abi::ERR_OUT_OF_RANGE => NetworkError::FrameTooLarge,
        native_abi::ERR_PEER_CLOSED => NetworkError::Disconnected,
        _ => NetworkError::Device,
    }
}
fn queue_request(ordinal: u32, queue: u16) -> Writer {
    let mut request = Writer::request(ordinal);
    request.u16(queue);
    request
}

fn encode_checksum(checksum: Option<NetworkChecksum>) -> u64 {
    checksum.map_or(0, |value| {
        (1u64 << 48)
            | u64::from(value.start)
            | (u64::from(value.offset) << 16)
            | (u64::from(value.header_length) << 32)
    })
}
fn decode_checksum(word: u64) -> Result<Option<NetworkChecksum>, Status> {
    if word == 0 {
        return Ok(None);
    }
    if word >> 48 != 1 {
        return Err(native_abi::ERR_INVALID_ARGS);
    }
    Ok(Some(NetworkChecksum {
        start: word as u16,
        offset: (word >> 16) as u16,
        header_length: (word >> 32) as u16,
    }))
}

fn encode_capabilities(writer: &mut Writer, cap: NetworkTransmitCapabilities) {
    writer
        .u8(u8::from(cap.checksum)
            | u8::from(cap.tcp_ipv4) << 1
            | u8::from(cap.tcp_ipv6) << 2
            | u8::from(cap.tcp_ecn) << 3)
        .u32(cap.maximum_frame_length)
        .u32(cap.maximum_buffer_length);
}
fn decode_capabilities(reader: &mut Reader<'_>) -> Result<NetworkTransmitCapabilities, Status> {
    let flags = reader.u8()?;
    if flags & !15 != 0 {
        return Err(native_abi::ERR_INVALID_ARGS);
    }
    let cap = NetworkTransmitCapabilities {
        checksum: flags & 1 != 0,
        tcp_ipv4: flags & 2 != 0,
        tcp_ipv6: flags & 4 != 0,
        tcp_ecn: flags & 8 != 0,
        maximum_frame_length: reader.u32()?,
        maximum_buffer_length: reader.u32()?,
    };
    cap.validate().map_err(encode_error)?;
    Ok(cap)
}
fn encode_segmentation(segmentation: Option<NetworkTcpSegmentation>) -> u64 {
    segmentation.map_or(0, |value| {
        u64::from(value.maximum_segment_size)
            | (match value.version {
                NetworkTcpVersion::Ipv4 => 1,
                NetworkTcpVersion::Ipv6 => 2,
            } << 16)
            | u64::from(value.ecn) << 18
    })
}
fn decode_segmentation(word: u64) -> Result<Option<NetworkTcpSegmentation>, Status> {
    if word == 0 {
        return Ok(None);
    }
    if word >> 19 != 0 {
        return Err(native_abi::ERR_INVALID_ARGS);
    }
    let version = match word >> 16 & 3 {
        1 => NetworkTcpVersion::Ipv4,
        2 => NetworkTcpVersion::Ipv6,
        _ => return Err(native_abi::ERR_INVALID_ARGS),
    };
    Ok(Some(NetworkTcpSegmentation {
        version,
        maximum_segment_size: word as u16,
        ecn: word >> 18 & 1 != 0,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn checksum_metadata_preserves_ranges_and_rejects_unknown_flags() {
        let checksum = NetworkChecksum {
            start: 34,
            offset: 16,
            header_length: 54,
        };
        assert_eq!(
            decode_checksum(encode_checksum(Some(checksum))),
            Ok(Some(checksum))
        );
        assert_eq!(decode_checksum(0), Ok(None));
        assert_eq!(decode_checksum(1 << 63), Err(native_abi::ERR_INVALID_ARGS));
        for version in [NetworkTcpVersion::Ipv4, NetworkTcpVersion::Ipv6] {
            let segmentation = NetworkTcpSegmentation {
                version,
                maximum_segment_size: 1440,
                ecn: true,
            };
            assert_eq!(
                decode_segmentation(encode_segmentation(Some(segmentation))),
                Ok(Some(segmentation))
            );
        }
        assert_eq!(decode_segmentation(0), Ok(None));
        assert_eq!(
            decode_segmentation(1 << 19),
            Err(native_abi::ERR_INVALID_ARGS)
        );
        assert_eq!(
            decode_segmentation(3 << 16),
            Err(native_abi::ERR_INVALID_ARGS)
        );
        assert!(checksum.validate(53).is_err());
        assert!(
            NetworkChecksum {
                header_length: 51,
                ..checksum
            }
            .validate(60)
            .is_err()
        );
    }
}
