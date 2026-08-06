//! Exact framed transport helpers shared by desktop and app roles.

use std::{
    io,
    os::unix::net::UnixStream,
    time::{SystemTime, UNIX_EPOCH},
};

use display_proto::{
    Accepted, Discarded, MessageKind, Presented, recv_message_owned, send_message,
};
use linux_uapi::drm::FlipEvent;

pub(super) fn receive(stream: &UnixStream) -> io::Result<(MessageKind, Vec<u8>)> {
    recv_message_owned(stream)?.ok_or_else(|| io::Error::from(io::ErrorKind::UnexpectedEof))
}

pub(super) fn send_accepted(stream: &UnixStream, revision: u64) -> io::Result<()> {
    let mut bytes = [0u8; 24];
    let message = Accepted { revision }
        .encode(&mut bytes)
        .ok_or_else(|| io::Error::other("accepted encoding failed"))?;
    send_message(stream, message)
}

pub(super) fn send_discarded(stream: &UnixStream, revision: u64) -> io::Result<()> {
    let mut bytes = [0u8; 24];
    let message = Discarded { revision }
        .encode(&mut bytes)
        .ok_or_else(|| io::Error::other("discarded encoding failed"))?;
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
        monotonic_ns: crate::frame_stats::flip_monotonic_ns(&event),
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
