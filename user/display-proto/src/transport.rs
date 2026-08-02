//! Display-protocol framing over Unix streams.

use std::{
    io::{self, Read, Write},
    os::unix::net::UnixStream,
};

/// Writes one complete protocol frame.
pub fn send_message(stream: &UnixStream, bytes: &[u8]) -> io::Result<()> {
    let mut stream = stream;
    stream.write_all(bytes)
}

/// Receives one complete blocking frame.
pub fn recv_frame_blocking(stream: &UnixStream, bytes: &mut [u8]) -> io::Result<usize> {
    if bytes.len() < crate::HEADER_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "display receive buffer is shorter than a frame header",
        ));
    }
    let mut filled = 0usize;
    loop {
        let target = if filled < crate::HEADER_LEN {
            crate::HEADER_LEN
        } else {
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
                return Ok(declared);
            }
            declared
        };
        let count = loop {
            match (&mut &*stream).read(&mut bytes[filled..target]) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                result => break result?,
            }
        };
        if count == 0 {
            return if filled == 0 {
                Ok(0)
            } else {
                Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "display frame ended early",
                ))
            };
        }
        filled += count;
    }
}
