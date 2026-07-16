use alloc::{boxed::Box, vec::Vec};

use crate::ffi;

const SOCKET_PATH: &[u8] = b"/run/liteui/compositor.sock\0";
const HEADER_BYTES: usize = 40;
const OPERATION_BYTES: usize = 40;
const MAX_PAYLOAD_BYTES: usize = 256 * 1024;
const MAX_OPERATIONS: u32 = 256;
const MAX_FRAME_BYTES: usize = HEADER_BYTES + MAX_PAYLOAD_BYTES;
const EVENT_BYTES: usize = 24;

#[derive(Clone, Copy)]
pub struct Event {
    pub node: u16,
    pub generation: u16,
}

/// Stable owner of the only host-to-compositor transport and pending frame.
///
/// QuickJS stores a raw pointer to this boxed value. The single bounded frame
/// is also the backpressure fact: JS cannot enqueue another transaction until
/// the compositor has consumed all bytes of the previous one.
pub struct Publisher {
    fd: i32,
    frame: Vec<u8>,
    sent: usize,
    next_sequence: u64,
    event: [u8; EVENT_BYTES],
    event_received: usize,
    next_event_sequence: u64,
}

impl Publisher {
    pub fn try_connect() -> Result<Box<Self>, ()> {
        let fd = unsafe { ffi::socket(ffi::AF_UNIX, ffi::SOCK_STREAM | ffi::SOCK_CLOEXEC, 0) };
        if fd < 0 {
            return Err(());
        }
        let mut address = ffi::SockaddrUn {
            family: ffi::AF_UNIX as u16,
            path: [0; 108],
        };
        address.path[..SOCKET_PATH.len()].copy_from_slice(SOCKET_PATH);
        let length = (core::mem::size_of::<u16>() + SOCKET_PATH.len() - 1) as u32;
        if unsafe { ffi::connect(fd, &address, length) } != 0
            || unsafe { ffi::fcntl(fd, ffi::F_SETFL, ffi::O_NONBLOCK) } != 0
        {
            unsafe { ffi::close(fd) };
            return Err(());
        }
        let mut frame = Vec::new();
        if frame.try_reserve_exact(MAX_FRAME_BYTES).is_err() {
            unsafe { ffi::close(fd) };
            return Err(());
        }
        Box::try_new(Self {
            fd,
            frame,
            sent: 0,
            next_sequence: 1,
            event: [0; EVENT_BYTES],
            event_received: 0,
            next_event_sequence: 1,
        })
        .map_err(|_| ())
    }

    pub fn queue(&mut self, payload: &[u8], operations: u32) -> i32 {
        let Some(expected) = usize::try_from(operations)
            .ok()
            .and_then(|count| count.checked_mul(OPERATION_BYTES))
        else {
            return -1;
        };
        if !self.frame.is_empty()
            || operations > MAX_OPERATIONS
            || payload.len() != expected
            || payload.len() > MAX_PAYLOAD_BYTES
        {
            return -1;
        }
        let Some(next_sequence) = self.next_sequence.checked_add(1) else {
            return -1;
        };
        self.frame.extend_from_slice(b"LUI1");
        append_u16(&mut self.frame, 1);
        append_u16(&mut self.frame, HEADER_BYTES as u16);
        append_u64(&mut self.frame, 1);
        append_u64(&mut self.frame, self.next_sequence);
        append_u32(&mut self.frame, operations);
        append_u32(&mut self.frame, payload.len() as u32);
        append_u32(&mut self.frame, 0);
        append_u32(&mut self.frame, 0);
        self.frame.extend_from_slice(payload);
        self.next_sequence = next_sequence;
        0
    }

    pub fn next_event(&mut self) -> Result<Event, ()> {
        loop {
            self.flush()?;
            let mut descriptor = ffi::PollFd {
                fd: self.fd,
                events: if self.frame.is_empty() {
                    ffi::POLLIN
                } else {
                    ffi::POLLIN | ffi::POLLOUT
                },
                returned: 0,
            };
            let ready = loop {
                let result = unsafe { ffi::poll(&mut descriptor, 1, -1) };
                if result < 0 && ffi::errno() == ffi::EINTR {
                    continue;
                }
                break result;
            };
            if ready < 0 || descriptor.returned & (ffi::POLLERR | ffi::POLLHUP) != 0 {
                return Err(());
            }
            if descriptor.returned & ffi::POLLIN != 0 {
                let count = unsafe {
                    ffi::read(
                        self.fd,
                        self.event[self.event_received..].as_mut_ptr().cast(),
                        EVENT_BYTES - self.event_received,
                    )
                };
                if count > 0 {
                    self.event_received =
                        self.event_received.checked_add(count as usize).ok_or(())?;
                    if self.event_received == EVENT_BYTES {
                        return self.take_event();
                    }
                } else if count < 0 && matches!(ffi::errno(), ffi::EINTR | ffi::EAGAIN) {
                    continue;
                } else {
                    return Err(());
                }
            }
        }
    }

    fn take_event(&mut self) -> Result<Event, ()> {
        let sequence = u64::from_le_bytes(self.event[16..24].try_into().map_err(|_| ())?);
        if &self.event[..4] != b"LUE1"
            || u16::from_le_bytes(self.event[4..6].try_into().map_err(|_| ())?) != 1
            || u16::from_le_bytes(self.event[6..8].try_into().map_err(|_| ())?) != 1
            || self.event[12..16] != [0, 0, 0, 0]
            || sequence != self.next_event_sequence
        {
            return Err(());
        }
        let next = self.next_event_sequence.checked_add(1).ok_or(())?;
        let event = Event {
            node: u16::from_le_bytes(self.event[8..10].try_into().map_err(|_| ())?),
            generation: u16::from_le_bytes(self.event[10..12].try_into().map_err(|_| ())?),
        };
        if event.node == 0 || event.generation == 0 {
            return Err(());
        }
        self.event_received = 0;
        self.next_event_sequence = next;
        Ok(event)
    }

    fn flush(&mut self) -> Result<(), ()> {
        while self.sent < self.frame.len() {
            let count = unsafe {
                ffi::send(
                    self.fd,
                    self.frame[self.sent..].as_ptr().cast(),
                    self.frame.len() - self.sent,
                    ffi::MSG_NOSIGNAL,
                )
            };
            if count > 0 {
                self.sent = self.sent.checked_add(count as usize).ok_or(())?;
            } else if count < 0 && ffi::errno() == ffi::EINTR {
                continue;
            } else if count < 0 && ffi::errno() == ffi::EAGAIN {
                return Ok(());
            } else {
                return Err(());
            }
        }
        self.frame.clear();
        self.sent = 0;
        Ok(())
    }
}

impl Drop for Publisher {
    fn drop(&mut self) {
        unsafe { ffi::close(self.fd) };
    }
}

fn append_u16(output: &mut Vec<u8>, value: u16) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn append_u32(output: &mut Vec<u8>, value: u32) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn append_u64(output: &mut Vec<u8>, value: u64) {
    output.extend_from_slice(&value.to_le_bytes());
}
