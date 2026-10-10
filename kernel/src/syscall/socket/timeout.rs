//! Linux LP64 socket timeval codec；仅产生语义 policy，不拥有 socket 或等待状态。
use super::errno;
use crate::socket::SocketTimeout;

pub(super) fn decode(bytes: [u8; 16]) -> Result<SocketTimeout, isize> {
    let seconds = i64::from_ne_bytes(bytes[..8].try_into().unwrap());
    let micros = i64::from_ne_bytes(bytes[8..].try_into().unwrap());
    if !(0..1_000_000).contains(&micros) {
        return Err(-errno::EDOM);
    }
    if seconds < 0 {
        return Ok(SocketTimeout::finite(0));
    }
    if seconds == 0 && micros == 0 {
        return Ok(SocketTimeout::Infinite);
    }
    Ok(SocketTimeout::finite(
        seconds as u128 * 1_000_000_000 + micros as u128 * 1_000,
    ))
}
pub(super) fn encode(timeout: SocketTimeout) -> [u8; 16] {
    let micros = timeout.microseconds();
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&((micros / 1_000_000) as i64).to_ne_bytes());
    bytes[8..].copy_from_slice(&((micros % 1_000_000) as i64).to_ne_bytes());
    bytes
}
