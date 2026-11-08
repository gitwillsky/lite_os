//! 以太网卡 v6：OPEN 显式协商 pair 数；一次授权 VMO，四条共享环传递收发，事件合并通知。
mod client;
mod queue;
mod rss;
mod server;

use crate::Writer;
pub use client::{RemoteNetwork, connect};
use device_model::network::NetworkError;
use native_runtime::native_abi::{self, Status};
pub use server::publish;

const NETWORK_VERSION: u16 = 6;
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
