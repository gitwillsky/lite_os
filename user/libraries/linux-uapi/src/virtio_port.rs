//! Safe access to the standard SPICE VirtIO serial port.

use std::{
    fs::{File, OpenOptions},
    io::{self, Read, Write},
    os::{
        fd::{AsFd, BorrowedFd},
        unix::fs::OpenOptionsExt,
    },
};

use crate::raw;

/// Standard Linux pathname for the SPICE vdagent port.
pub const SPICE_PORT_PATH: &str = "/dev/virtio-ports/com.redhat.spice.0";

/// Nonblocking, close-on-exec SPICE byte stream.
pub struct SpicePort {
    file: File,
}

impl SpicePort {
    /// Opens the standard named VirtIO port for bidirectional agent traffic.
    pub fn open() -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(raw::O_NONBLOCK | raw::O_CLOEXEC)
            .open(SPICE_PORT_PATH)?;
        Ok(Self { file })
    }

    /// Reads currently available bytes without blocking.
    pub fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        self.file.read(output)
    }

    /// Writes as much queued protocol data as the device currently accepts.
    pub fn write(&mut self, input: &[u8]) -> io::Result<usize> {
        self.file.write(input)
    }
}

impl AsFd for SpicePort {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.file.as_fd()
    }
}
