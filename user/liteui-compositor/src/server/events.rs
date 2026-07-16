use crate::ffi;

const EVENT_BYTES: usize = 24;
const EVENT_CAPACITY: usize = 64;

/// Fixed compositor-to-client queue; input publication never allocates.
pub(super) struct Queue {
    frames: [[u8; EVENT_BYTES]; EVENT_CAPACITY],
    head: usize,
    length: usize,
    sent: usize,
    next_sequence: u64,
}

impl Queue {
    pub(super) const fn new() -> Self {
        Self {
            frames: [[0; EVENT_BYTES]; EVENT_CAPACITY],
            head: 0,
            length: 0,
            sent: 0,
            next_sequence: 1,
        }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.length == 0
    }

    pub(super) fn remaining(&self) -> usize {
        EVENT_CAPACITY - self.length
    }

    pub(super) fn push(&mut self, kind: u16, payload: [u8; 8]) -> Result<bool, ()> {
        if self.length == EVENT_CAPACITY {
            return Ok(false);
        }
        let next = self.next_sequence.checked_add(1).ok_or(())?;
        let index = (self.head + self.length) % EVENT_CAPACITY;
        let frame = &mut self.frames[index];
        frame.fill(0);
        frame[..4].copy_from_slice(b"LUE1");
        frame[4..6].copy_from_slice(&1u16.to_le_bytes());
        frame[6..8].copy_from_slice(&kind.to_le_bytes());
        frame[8..16].copy_from_slice(&payload);
        frame[16..24].copy_from_slice(&self.next_sequence.to_le_bytes());
        self.length += 1;
        self.next_sequence = next;
        Ok(true)
    }

    pub(super) fn flush(&mut self, fd: i32) -> Result<(), ()> {
        while self.length != 0 {
            let frame = &self.frames[self.head];
            let count = unsafe {
                ffi::send(
                    fd,
                    frame[self.sent..].as_ptr().cast(),
                    EVENT_BYTES - self.sent,
                    ffi::MSG_NOSIGNAL,
                )
            };
            if count > 0 {
                self.sent = self.sent.checked_add(count as usize).ok_or(())?;
                if self.sent == EVENT_BYTES {
                    self.sent = 0;
                    self.head = (self.head + 1) % EVENT_CAPACITY;
                    self.length -= 1;
                }
            } else if count < 0 && ffi::errno() == ffi::EINTR {
                continue;
            } else if count < 0 && ffi::errno() == ffi::EAGAIN {
                return Ok(());
            } else {
                return Err(());
            }
        }
        Ok(())
    }
}
