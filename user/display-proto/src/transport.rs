//! Display-protocol framing over Unix streams, including one `SCM_RIGHTS` fd.

use std::{
    io::{self, Write},
    os::{
        fd::{AsFd, BorrowedFd, OwnedFd},
        unix::net::UnixStream,
    },
};

/// Writes one complete protocol frame.
pub fn send_message(stream: &UnixStream, bytes: &[u8]) -> io::Result<()> {
    let mut stream = stream;
    stream.write_all(bytes)
}

/// Writes one complete frame with a single `SCM_RIGHTS` descriptor.
pub fn send_message_with_fd(
    stream: &UnixStream,
    bytes: &[u8],
    fd: BorrowedFd<'_>,
) -> io::Result<()> {
    loop {
        match linux_uapi::unix::send_fd(stream.as_fd(), bytes, fd) {
            Ok(count) if count == bytes.len() => return Ok(()),
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "SCM_RIGHTS frame was partially written",
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
}

/// Receives one stream chunk and at most one owned `SCM_RIGHTS` descriptor.
pub fn recv_message(stream: &UnixStream, bytes: &mut [u8]) -> io::Result<(usize, Option<OwnedFd>)> {
    linux_uapi::unix::recv_fd(stream.as_fd(), bytes)
}

/// Receives one complete blocking frame while absorbing the SCM barrier split.
pub fn recv_frame_blocking(
    stream: &UnixStream,
    bytes: &mut [u8],
) -> io::Result<(usize, Option<OwnedFd>)> {
    let mut received_fd = None;
    let mut filled = 0usize;
    loop {
        if filled >= crate::HEADER_LEN {
            let declared = u32::from_le_bytes(
                bytes[..4]
                    .try_into()
                    .expect("frame header has four length bytes"),
            ) as usize;
            if !(crate::HEADER_LEN..=crate::MAX_MESSAGE).contains(&declared)
                || declared > bytes.len()
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid display frame length",
                ));
            }
            if filled >= declared {
                return Ok((declared, received_fd));
            }
        }
        let (count, next_fd) = loop {
            match recv_message(stream, &mut bytes[filled..]) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                result => break result?,
            }
        };
        if let Some(next_fd) = next_fd
            && received_fd.replace(next_fd).is_some()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "display frame carried multiple descriptors",
            ));
        }
        if count == 0 {
            return if filled == 0 {
                Ok((0, received_fd))
            } else {
                Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "display frame ended early",
                ))
            };
        }
        filled += count;
        if filled == bytes.len() && crate::parse_header(&bytes[..filled]).is_none() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "display frame exceeds receive buffer",
            ));
        }
    }
}
