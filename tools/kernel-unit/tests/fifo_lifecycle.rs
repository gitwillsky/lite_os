#![feature(allocator_ext)]
#![allow(dead_code, unused_imports)]
//! 实际 FIFO file/session 与 Pipe admission；user-copy 和 ioctl 内存是显式边界。
extern crate alloc;
#[path = "../../../kernel/src/fallible_tree/mod.rs"]
mod fallible_tree;
#[path = "../../../kernel/src/id.rs"]
mod id;
#[path = "../src/pipe_fixture.rs"]
mod ipc;

#[path = "../../../kernel/src/fs"]
mod fs {
    pub(crate) mod fifo;
    pub(crate) mod chardev {
        use alloc::sync::Arc;
        #[derive(Debug, PartialEq, Eq)]
        pub(crate) enum DeviceError {
            Errno(isize),
            WouldBlock,
            BrokenPipe,
        }
        pub(crate) struct UserFault;
        pub(crate) trait UserOutput {
            fn remaining(&self) -> usize;
            fn write(&mut self, bytes: &[u8]) -> Result<(), UserFault>;
        }
        pub(crate) trait UserInput {
            fn remaining(&self) -> usize;
            fn copy(&self, bytes: &mut [u8]) -> Result<(), UserFault>;
            fn consume(&mut self, length: usize);
        }
        pub(crate) trait UserMemory {
            fn write(&self, address: usize, bytes: &[u8]) -> Result<(), UserFault>;
        }
        pub(crate) struct IoctlCall<'a> {
            pub(crate) request: usize,
            pub(crate) argument: usize,
            pub(crate) user: &'a dyn UserMemory,
        }
        pub(crate) enum DeviceWaitSource {
            Pipe {
                pipe: Arc<crate::ipc::Pipe>,
                direction: crate::ipc::PipeDirection,
                events: i16,
            },
        }
        pub(crate) struct DeviceWaitSources;
        impl DeviceWaitSources {
            pub(crate) fn new() -> Self {
                Self
            }
            pub(crate) fn push(&mut self, _: DeviceWaitSource) {}
        }
        pub(crate) trait DeviceFile: Send + Sync {
            fn release(&self);
            fn read(
                &self,
                output: &mut dyn UserOutput,
                nonblocking: bool,
            ) -> Result<(), DeviceError>;
            fn write(
                &self,
                input: &mut dyn UserInput,
                nonblocking: bool,
            ) -> Result<(), DeviceError>;
            fn poll(&self, events: i16) -> i16;
            fn wait_sources(&self, events: i16) -> DeviceWaitSources;
            fn backing_pipe(&self) -> Option<Arc<crate::ipc::Pipe>>;
            fn ioctl(&self, call: &IoctlCall<'_>) -> Result<isize, DeviceError>;
            fn readiness_generation(&self) -> u64;
        }
    }
}
use fs::{
    chardev::*,
    fifo::{self, FifoAccess},
};
struct Output {
    buffer: [u8; 32],
    length: usize,
}
impl Output {
    fn new() -> Self {
        Self {
            buffer: [0; 32],
            length: 0,
        }
    }
}
impl UserOutput for Output {
    fn remaining(&self) -> usize {
        self.buffer.len() - self.length
    }
    fn write(&mut self, bytes: &[u8]) -> Result<(), UserFault> {
        self.buffer[self.length..self.length + bytes.len()].copy_from_slice(bytes);
        self.length += bytes.len();
        Ok(())
    }
}
struct Input<'a>(&'a [u8]);
impl UserInput for Input<'_> {
    fn remaining(&self) -> usize {
        self.0.len()
    }
    fn copy(&self, bytes: &mut [u8]) -> Result<(), UserFault> {
        bytes.copy_from_slice(&self.0[..bytes.len()]);
        Ok(())
    }
    fn consume(&mut self, length: usize) {
        self.0 = &self.0[length..];
    }
}
#[test]
fn actual_writer_fput_publishes_eof_despite_backend_observer_and_is_idempotent() {
    let reader = fifo::open((994, 1), FifoAccess::Read, true).unwrap();
    let writer = fifo::open((994, 1), FifoAccess::Write, true).unwrap();
    let observer = writer.clone();
    writer.release();
    drop(writer);
    let mut output = Output::new();
    reader.read(&mut output, true).unwrap();
    assert_eq!(output.length, 0);
    assert_ne!(reader.poll(1) & 0x10, 0);
    let replacement = fifo::open((994, 1), FifoAccess::Write, true).unwrap();
    observer.release();
    drop(observer);
    replacement.write(&mut Input(b"replacement"), true).unwrap();
    reader.read(&mut output, true).unwrap();
    assert_eq!(&output.buffer[..output.length], b"replacement");
    replacement.release();
    reader.release();
}
#[test]
fn last_actual_close_discards_ring_and_reopen_never_revives_observed_session() {
    let file = fifo::open((994, 2), FifoAccess::ReadWrite, true).unwrap();
    let observer = file.clone();
    let old_pipe = file.backing_pipe().unwrap();
    file.write(&mut Input(b"old bytes"), true).unwrap();
    assert_ne!(old_pipe.buffered_bytes(), 0);
    file.release();
    assert!(old_pipe.retired());
    assert_eq!(old_pipe.buffered_bytes(), 0);
    let replacement = fifo::open((994, 2), FifoAccess::ReadWrite, true).unwrap();
    assert!(!alloc::sync::Arc::ptr_eq(
        &old_pipe,
        &replacement.backing_pipe().unwrap()
    ));
    observer.release();
    drop(file);
    drop(observer);
    replacement.write(&mut Input(b"new bytes"), true).unwrap();
    let mut output = Output::new();
    replacement.read(&mut output, true).unwrap();
    assert_eq!(&output.buffer[..output.length], b"new bytes");
    replacement.release();
}
#[test]
fn reader_fput_immediately_breaks_writer_and_next_session_keeps_native_open_rules() {
    let reader = fifo::open((994, 3), FifoAccess::Read, true).unwrap();
    let writer = fifo::open((994, 3), FifoAccess::Write, true).unwrap();
    let observer = reader.clone();
    reader.release();
    assert_eq!(
        writer.write(&mut Input(b"broken"), true),
        Err(DeviceError::BrokenPipe)
    );
    assert_ne!(writer.poll(4) & 8, 0);
    writer.release();
    assert!(matches!(
        fifo::open((994, 3), FifoAccess::Write, true),
        Err(fifo::FifoOpenError::NoReader)
    ));
    drop(observer);
    drop(reader);
    drop(writer);
    let replacement = fifo::open((994, 3), FifoAccess::Read, true).unwrap();
    assert_eq!(
        replacement.poll(1) & 0x10,
        0,
        "new named reader suppresses HUP until a writer opens"
    );
    replacement.release();
}

#[path = "."]
pub(crate) mod execution {
    #[path = "../src/sync.rs"]
    pub(crate) mod sync;
}
