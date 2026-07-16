use alloc::vec::Vec;

use crate::{
    ffi,
    model::{Grid, Model},
};

const SOCKET_PATH: &[u8] = b"/run/liteui/compositor.sock\0";
const HEADER_BYTES: usize = 40;
const CELL_BYTES: usize = 16;
const EVENT_BYTES: usize = 24;
pub const GRID_CAPACITY: usize = 16_384;

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Configuration {
    pub columns: u16,
    pub rows: u16,
    pub pixel_width: u16,
    pub pixel_height: u16,
}

pub enum Event {
    Key {
        code: u16,
        value: i32,
    },
    Configure(Configuration),
    Pointer {
        button: u8,
        pressed: bool,
        column: u16,
        row: u16,
    },
}

/// Sole owner of terminal-service transport, sequence and bounded frame storage.
pub struct Connection {
    fd: i32,
    frame: Vec<u8>,
    sent: usize,
    next_sequence: u64,
    event: [u8; EVENT_BYTES],
    event_received: usize,
    next_event_sequence: u64,
}

impl Connection {
    pub fn connect() -> Result<Self, ()> {
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
        frame
            .try_reserve_exact(HEADER_BYTES + GRID_CAPACITY * CELL_BYTES)
            .map_err(|_| ())?;
        Ok(Self {
            fd,
            frame,
            sent: 0,
            next_sequence: 1,
            event: [0; EVENT_BYTES],
            event_received: 0,
            next_event_sequence: 1,
        })
    }

    pub fn fd(&self) -> i32 {
        self.fd
    }

    pub fn poll_events(&self, allow_input: bool) -> i16 {
        (if allow_input { ffi::POLLIN } else { 0 })
            | if self.frame.is_empty() {
                0
            } else {
                ffi::POLLOUT
            }
    }

    pub fn can_publish(&self) -> bool {
        self.frame.is_empty()
    }

    pub fn queue_grid(&mut self, model: &Model) -> Result<(), ()> {
        if !self.frame.is_empty() {
            return Err(());
        }
        let count = model.columns().checked_mul(model.rows()).ok_or(())?;
        if count == 0 || count > GRID_CAPACITY {
            return Err(());
        }
        let next = self.next_sequence.checked_add(1).ok_or(())?;
        self.frame.extend_from_slice(b"LUG1");
        append_u16(&mut self.frame, 1);
        append_u16(&mut self.frame, HEADER_BYTES as u16);
        append_u64(&mut self.frame, 1);
        append_u64(&mut self.frame, self.next_sequence);
        append_u16(&mut self.frame, model.columns() as u16);
        append_u16(&mut self.frame, model.rows() as u16);
        append_u32(&mut self.frame, (count * CELL_BYTES) as u32);
        let cursor = model.cursor();
        append_u16(
            &mut self.frame,
            cursor.map_or(u16::MAX, |(_, column)| column as u16),
        );
        append_u16(
            &mut self.frame,
            cursor.map_or(u16::MAX, |(row, _)| row as u16),
        );
        append_u16(
            &mut self.frame,
            u16::from(model.reverse_screen()) | u16::from(model.blink_visible()) << 1,
        );
        append_u16(&mut self.frame, 0);
        for row in 0..model.rows() {
            for column in 0..model.columns() {
                let cell = model.cell(row, column);
                append_u32(&mut self.frame, cell.codepoint);
                append_u32(&mut self.frame, cell.foreground);
                append_u32(&mut self.frame, cell.background);
                append_u16(&mut self.frame, cell.attributes);
                append_u16(&mut self.frame, 0);
            }
        }
        self.next_sequence = next;
        Ok(())
    }

    pub fn flush(&mut self) -> Result<(), ()> {
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

    pub fn read_event(&mut self) -> Result<Option<Event>, ()> {
        while self.event_received < EVENT_BYTES {
            let count = unsafe {
                ffi::read(
                    self.fd,
                    self.event[self.event_received..].as_mut_ptr().cast(),
                    EVENT_BYTES - self.event_received,
                )
            };
            if count > 0 {
                self.event_received = self.event_received.checked_add(count as usize).ok_or(())?;
            } else if count < 0 && ffi::errno() == ffi::EINTR {
                continue;
            } else if count < 0 && ffi::errno() == ffi::EAGAIN {
                return Ok(None);
            } else {
                return Err(());
            }
        }
        let sequence = read_u64(&self.event, 16)?;
        if &self.event[..4] != b"LUE1"
            || read_u16(&self.event, 4)? != 1
            || sequence != self.next_event_sequence
        {
            return Err(());
        }
        let next = self.next_event_sequence.checked_add(1).ok_or(())?;
        let event = match read_u16(&self.event, 6)? {
            2 if self.event[14..16] == [0, 0] => Event::Key {
                code: read_u16(&self.event, 8)?,
                value: read_i32(&self.event, 10)?,
            },
            3 => Event::Configure(Configuration {
                columns: read_u16(&self.event, 8)?,
                rows: read_u16(&self.event, 10)?,
                pixel_width: read_u16(&self.event, 12)?,
                pixel_height: read_u16(&self.event, 14)?,
            }),
            5 if self.event[14..16] == [0, 0] && self.event[9] <= 1 => Event::Pointer {
                button: self.event[8],
                pressed: self.event[9] != 0,
                column: read_u16(&self.event, 10)?,
                row: read_u16(&self.event, 12)?,
            },
            _ => return Err(()),
        };
        self.event_received = 0;
        self.next_event_sequence = next;
        Ok(Some(event))
    }
}

impl Drop for Connection {
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

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, ()> {
    Ok(u16::from_le_bytes(
        bytes
            .get(offset..offset + 2)
            .ok_or(())?
            .try_into()
            .map_err(|_| ())?,
    ))
}

fn read_i32(bytes: &[u8], offset: usize) -> Result<i32, ()> {
    Ok(i32::from_le_bytes(
        bytes
            .get(offset..offset + 4)
            .ok_or(())?
            .try_into()
            .map_err(|_| ())?,
    ))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, ()> {
    Ok(u64::from_le_bytes(
        bytes
            .get(offset..offset + 8)
            .ok_or(())?
            .try_into()
            .map_err(|_| ())?,
    ))
}
