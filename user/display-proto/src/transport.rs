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

/// Receives one complete message into payload storage sized from its validated header.
///
/// # Parameters
///
/// - `stream`: Connected display-protocol stream to read.
///
/// # Returns
///
/// The exact kind and owned payload, or `None` when the peer closes before a
/// new message starts.
///
/// # Errors
///
/// Returns an error for a truncated header/body, a declared length outside the
/// protocol quota, allocation failure, or an underlying stream read failure.
pub fn recv_message_owned(
    stream: &UnixStream,
) -> io::Result<Option<(crate::MessageKind, Vec<u8>)>> {
    let mut header = [0u8; crate::HEADER_LEN];
    let mut stream = stream;
    let first = loop {
        match stream.read(&mut header[..1]) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            result => break result?,
        }
    };
    if first == 0 {
        return Ok(None);
    }
    stream.read_exact(&mut header[1..])?;
    let declared = u32::from_le_bytes(
        header[..4]
            .try_into()
            .expect("frame header has four length bytes"),
    ) as usize;
    if !(crate::HEADER_LEN..=crate::MAX_MESSAGE).contains(&declared) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid display frame length",
        ));
    }
    let kind = crate::MessageKind::from_raw(u32::from_le_bytes(
        header[4..8]
            .try_into()
            .expect("frame header has four kind bytes"),
    ))
    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid display message kind"))?;
    let payload_len = declared - crate::HEADER_LEN;
    let mut payload = Vec::new();
    payload
        .try_reserve_exact(payload_len)
        .map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))?;
    let mut body = stream.take(payload_len as u64);
    body.read_to_end(&mut payload)?;
    if payload.len() != payload_len {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "display frame ended early",
        ));
    }
    Ok(Some((kind, payload)))
}

#[cfg(test)]
mod tests {
    use std::{os::unix::net::UnixStream, thread};

    use crate::{MAX_CONTROL_MESSAGE, MessageKind};

    use super::{recv_message_owned, send_message};

    #[test]
    fn owned_receive_accepts_display_frame_larger_than_control_quota() {
        let (sender, receiver) = UnixStream::pair().expect("socket pair");
        let length = MAX_CONTROL_MESSAGE + 4096;
        let mut frame = vec![0u8; length];
        frame[..4].copy_from_slice(&(length as u32).to_le_bytes());
        frame[4..8].copy_from_slice(&(MessageKind::DisplayListCommit as u32).to_le_bytes());
        let writer = thread::spawn(move || send_message(&sender, &frame));

        let (kind, payload) = recv_message_owned(&receiver)
            .expect("large frame")
            .expect("message before EOF");
        writer.join().expect("writer thread").expect("frame write");
        assert_eq!(kind, MessageKind::DisplayListCommit);
        assert_eq!(payload.len(), length - crate::HEADER_LEN);
    }
}
