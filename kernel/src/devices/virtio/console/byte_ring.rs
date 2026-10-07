//! Fixed-capacity receive storage for the VirtIO Console byte stream.

use alloc::{boxed::Box, vec::Vec};

const STREAM_CAPACITY: usize = 64 * 1024;

pub(super) struct ByteRing {
    bytes: Box<[u8]>,
    head: usize,
    length: usize,
}

impl ByteRing {
    pub(super) fn new() -> Option<Self> {
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(STREAM_CAPACITY).ok()?;
        bytes.resize(STREAM_CAPACITY, 0);
        Some(Self {
            bytes: bytes.into_boxed_slice(),
            head: 0,
            length: 0,
        })
    }

    pub(super) fn push(&mut self, input: &[u8]) -> bool {
        if input.len() > self.bytes.len() - self.length {
            return false;
        }
        let tail = (self.head + self.length) % self.bytes.len();
        let first = input.len().min(self.bytes.len() - tail);
        self.bytes[tail..tail + first].copy_from_slice(&input[..first]);
        self.bytes[..input.len() - first].copy_from_slice(&input[first..]);
        self.length += input.len();
        true
    }

    pub(super) fn pop(&mut self, output: &mut [u8]) -> usize {
        let count = output.len().min(self.length);
        let first = count.min(self.bytes.len() - self.head);
        output[..first].copy_from_slice(&self.bytes[self.head..self.head + first]);
        output[first..count].copy_from_slice(&self.bytes[..count - first]);
        self.head = (self.head + count) % self.bytes.len();
        self.length -= count;
        count
    }

    pub(super) fn is_empty(&self) -> bool {
        self.length == 0
    }
}
