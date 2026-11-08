//! 以太网卡 v7：OPEN 显式协商 pair 数；一次授权 VMO，四条共享环传递收发，事件合并通知。
mod client;
mod queue;
mod rss;
mod server;

use crate::Writer;
pub use client::{RemoteNetwork, connect};
use device_model::network::{NetworkChecksum, NetworkError};
use native_runtime::native_abi::{self, Status};
pub use server::publish;

const NETWORK_VERSION: u16 = 7;
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
