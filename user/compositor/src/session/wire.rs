//! Exact framed transport helpers shared by desktop and app roles.

use std::{
    io,
    os::unix::net::UnixStream,
    time::{SystemTime, UNIX_EPOCH},
};

use display_proto::{
    Accepted, MAX_MESSAGE, MessageKind, Presented, parse_frame, recv_frame_blocking, send_message,
};
use linux_uapi::drm::FlipEvent;

use super::invalid;

pub(super) fn receive(stream: &UnixStream) -> io::Result<(MessageKind, Vec<u8>)> {
    let mut bytes = vec![0u8; MAX_MESSAGE];
    let (length, fd) = recv_frame_blocking(stream, &mut bytes)?;
    if length == 0 {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "display EOF"));
    }
    if fd.is_some() {
        return Err(invalid("unexpected descriptor"));
    }
    let frame = parse_frame(&bytes[..length]).ok_or_else(|| invalid("invalid display frame"))?;
    Ok((frame.kind(), frame.payload().to_vec()))
}

pub(super) fn send_accepted(stream: &UnixStream, revision: u64) -> io::Result<()> {
    let mut bytes = [0u8; 24];
    let message = Accepted { revision }
        .encode(&mut bytes)
        .ok_or_else(|| io::Error::other("accepted encoding failed"))?;
    send_message(stream, message)
}

pub(super) fn send_presented(
    stream: &UnixStream,
    revision: u64,
    event: FlipEvent,
) -> io::Result<()> {
    let mut bytes = [0u8; 48];
    let message = Presented {
        revision,
        frame_sequence: u64::from(event.sequence),
        monotonic_ns: u64::from(event.seconds) * 1_000_000_000
            + u64::from(event.microseconds) * 1_000,
    }
    .encode(&mut bytes)
    .ok_or_else(|| io::Error::other("presented encoding failed"))?;
    send_message(stream, message)
}

pub(super) fn valid_app_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 63
        && id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

pub(super) fn new_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64
}
