use std::{
    io::{self, Write},
    os::{
        fd::{AsFd, BorrowedFd, OwnedFd},
        unix::net::UnixStream,
    },
};

use crate::{
    ClientMessage, FRAME_HEADER_LEN, MAX_FRAME_LEN, ServiceMessage, decode_client, decode_service,
    encode_client, encode_service,
};

/// Result of one nonblocking client receive attempt.
#[derive(Debug, PartialEq)]
pub enum ClientReceive {
    /// No complete frame is available yet.
    Pending,
    /// The peer closed cleanly between frames.
    Closed,
    /// One complete typed frame is ready.
    Message(ClientMessage),
}

/// Incremental, allocation-free receiver for a nonblocking service connection.
pub struct ClientFrameReceiver {
    bytes: [u8; MAX_FRAME_LEN],
    filled: usize,
}

impl ClientFrameReceiver {
    /// Creates an empty v1 frame receiver.
    pub const fn new() -> Self {
        Self {
            bytes: [0; MAX_FRAME_LEN],
            filled: 0,
        }
    }

    /// Receives at most one socket chunk and preserves an incomplete frame.
    ///
    /// # Parameters
    ///
    /// - `stream`: One nonblocking client control connection.
    ///
    /// # Returns
    ///
    /// The completed message, clean EOF, or `Pending` for partial/no input.
    ///
    /// # Errors
    ///
    /// Returns invalid-data for malformed lengths, frames, or any client fd.
    pub fn receive(&mut self, stream: &UnixStream) -> io::Result<ClientReceive> {
        let target = if self.filled < FRAME_HEADER_LEN {
            FRAME_HEADER_LEN
        } else {
            let declared = u32::from_le_bytes(
                self.bytes[..4]
                    .try_into()
                    .expect("frame header has four length bytes"),
            ) as usize;
            if !(FRAME_HEADER_LEN..=MAX_FRAME_LEN).contains(&declared) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid audio frame length",
                ));
            }
            if self.filled == declared {
                let message = decode_client(&self.bytes[..declared]).map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidData, "invalid audio client frame")
                })?;
                self.filled = 0;
                return Ok(ClientReceive::Message(message));
            }
            declared
        };
        let received =
            linux_uapi::unix::recv_fd(stream.as_fd(), &mut self.bytes[self.filled..target]);
        let (count, fd) = match received {
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                return Ok(ClientReceive::Pending);
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                return Ok(ClientReceive::Pending);
            }
            result => result?,
        };
        if fd.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "client control frame carried a descriptor",
            ));
        }
        if count == 0 {
            return if self.filled == 0 {
                Ok(ClientReceive::Closed)
            } else {
                Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "audio frame ended before its declared length",
                ))
            };
        }
        self.filled += count;
        let completed = if self.filled < FRAME_HEADER_LEN {
            false
        } else {
            let declared = u32::from_le_bytes(
                self.bytes[..4]
                    .try_into()
                    .expect("frame header has four length bytes"),
            ) as usize;
            if !(FRAME_HEADER_LEN..=MAX_FRAME_LEN).contains(&declared) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid audio frame length",
                ));
            }
            self.filled == declared
        };
        if completed {
            let target = self.filled;
            let message = decode_client(&self.bytes[..target]).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "invalid audio client frame")
            })?;
            self.filled = 0;
            Ok(ClientReceive::Message(message))
        } else {
            Ok(ClientReceive::Pending)
        }
    }
}

impl Default for ClientFrameReceiver {
    fn default() -> Self {
        Self::new()
    }
}

fn receive_frame(
    stream: &UnixStream,
    bytes: &mut [u8; MAX_FRAME_LEN],
) -> io::Result<Option<(usize, Option<OwnedFd>)>> {
    let mut filled = 0usize;
    let mut received_fd = None;
    loop {
        let target = if filled < FRAME_HEADER_LEN {
            FRAME_HEADER_LEN
        } else {
            let declared = u32::from_le_bytes(
                bytes[..4]
                    .try_into()
                    .expect("frame header has four length bytes"),
            ) as usize;
            if !(FRAME_HEADER_LEN..=MAX_FRAME_LEN).contains(&declared) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid audio frame length",
                ));
            }
            if filled >= declared {
                return Ok(Some((declared, received_fd)));
            }
            declared
        };
        let (count, next_fd) = loop {
            match linux_uapi::unix::recv_fd(stream.as_fd(), &mut bytes[filled..target]) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                result => break result?,
            }
        };
        if let Some(next_fd) = next_fd
            && received_fd.replace(next_fd).is_some()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "audio frame carried multiple descriptors",
            ));
        }
        if count == 0 {
            return if filled == 0 && received_fd.is_none() {
                Ok(None)
            } else {
                Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "audio frame ended before its declared length",
                ))
            };
        }
        filled += count;
    }
}

/// Sends one typed client frame without ancillary data.
pub fn send_client(stream: &UnixStream, message: ClientMessage) -> io::Result<()> {
    let mut bytes = [0; MAX_FRAME_LEN];
    let frame = encode_client(message, &mut bytes)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "audio frame overflow"))?;
    let mut stream = stream;
    stream.write_all(frame.as_bytes())
}

/// Receives one typed client frame and rejects every descriptor.
pub fn recv_client(
    stream: &UnixStream,
    bytes: &mut [u8; MAX_FRAME_LEN],
) -> io::Result<Option<ClientMessage>> {
    let Some((length, fd)) = receive_frame(stream, bytes)? else {
        return Ok(None);
    };
    if fd.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "client control frame carried a descriptor",
        ));
    }
    decode_client(&bytes[..length])
        .map(Some)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid audio client frame"))
}

/// Sends one typed service frame, enforcing the sole `STREAM_CREATED` fd publication.
pub fn send_service(
    stream: &UnixStream,
    message: ServiceMessage,
    fd: Option<BorrowedFd<'_>>,
) -> io::Result<()> {
    let expects_fd = matches!(message, ServiceMessage::StreamCreated { .. });
    if expects_fd != fd.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "only STREAM_CREATED publishes exactly one ring descriptor",
        ));
    }
    let mut bytes = [0; MAX_FRAME_LEN];
    let frame = encode_service(message, &mut bytes)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "audio frame overflow"))?;
    if let Some(fd) = fd {
        loop {
            match linux_uapi::unix::send_fd(stream.as_fd(), frame.as_bytes(), fd) {
                Ok(count) if count == frame.as_bytes().len() => return Ok(()),
                Ok(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "SCM_RIGHTS audio frame was partially written",
                    ));
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
    }
    let mut stream = stream;
    stream.write_all(frame.as_bytes())
}

/// Receives one typed service frame and its required stream-created memfd.
pub fn recv_service(
    stream: &UnixStream,
    bytes: &mut [u8; MAX_FRAME_LEN],
) -> io::Result<Option<(ServiceMessage, Option<OwnedFd>)>> {
    let Some((length, fd)) = receive_frame(stream, bytes)? else {
        return Ok(None);
    };
    let message = decode_service(&bytes[..length])
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid audio service frame"))?;
    let expects_fd = matches!(message, ServiceMessage::StreamCreated { .. });
    if expects_fd != fd.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid audio service descriptor publication",
        ));
    }
    Ok(Some((message, fd)))
}

#[cfg(test)]
mod tests {
    use std::os::unix::net::UnixStream;

    use super::*;
    use crate::RING_CAPACITY_FRAMES;

    #[cfg(target_os = "linux")]
    #[test]
    fn transport_preserves_frames_and_single_fd_barrier() {
        use std::fs::File;

        use crate::PROTOCOL_VERSION;
        let (service, client) = UnixStream::pair().expect("pair");
        let file = File::open("/dev/null").expect("open");
        send_service(
            &service,
            ServiceMessage::StreamCreated {
                stream_id: 9,
                generation: 3,
                capacity_frames: RING_CAPACITY_FRAMES as u32,
            },
            Some(file.as_fd()),
        )
        .expect("send");
        let mut bytes = [0; MAX_FRAME_LEN];
        let (message, received) = recv_service(&client, &mut bytes)
            .expect("receive")
            .expect("not eof");
        assert_eq!(
            message,
            ServiceMessage::StreamCreated {
                stream_id: 9,
                generation: 3,
                capacity_frames: RING_CAPACITY_FRAMES as u32,
            }
        );
        assert!(received.is_some());

        send_client(
            &client,
            ClientMessage::Hello {
                version: PROTOCOL_VERSION,
                role: crate::ClientRole::Media,
            },
        )
        .expect("send hello");
        assert_eq!(
            recv_client(&service, &mut bytes).expect("receive hello"),
            Some(ClientMessage::Hello {
                version: PROTOCOL_VERSION,
                role: crate::ClientRole::Media,
            })
        );
    }

    #[test]
    fn stream_created_requires_exactly_one_descriptor() {
        let (service, _client) = UnixStream::pair().expect("pair");
        assert_eq!(
            send_service(
                &service,
                ServiceMessage::StreamCreated {
                    stream_id: 1,
                    generation: 1,
                    capacity_frames: RING_CAPACITY_FRAMES as u32,
                },
                None,
            )
            .expect_err("missing fd")
            .kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn incremental_receiver_completes_header_only_frame() {
        let (service, client) = UnixStream::pair().expect("pair");
        client.set_nonblocking(true).expect("nonblocking");
        send_client(&service, ClientMessage::GetMasterState).expect("send");
        let mut receiver = ClientFrameReceiver::new();
        assert_eq!(
            receiver.receive(&client).expect("receive"),
            ClientReceive::Message(ClientMessage::GetMasterState)
        );
    }
}
