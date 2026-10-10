//! Production transfer order; deterministic input/storage and caller-limit seams.
#![allow(dead_code)]
use std::cell::Cell;
mod memory {
    pub(crate) const PAGE_SIZE: usize = 4096;
}
mod errno {
    pub(crate) const EINVAL: isize = 22;
}
#[derive(Clone, Copy, Debug)]
enum FileSystemError {
    Io,
}
fn ferr(_: FileSystemError) -> isize {
    -5
}
struct AccessIdentity;
struct TaskControlBlock;
impl TaskControlBlock {
    fn access_identity(&self, _: bool) -> AccessIdentity {
        AccessIdentity
    }
    fn account_read_storage(&self, _: usize) {}
    fn account_write_storage(&self, _: usize) {}
}
fn bounded_regular_write(
    _: &TaskControlBlock,
    _: u64,
    count: usize,
    _: usize,
) -> Result<usize, isize> {
    Ok(count)
}
#[derive(Clone, Copy)]
enum ReadPlan {
    Success,
    Error,
    Eof,
    SecondError,
}
struct RegularFile {
    plan: ReadPlan,
    reads: Cell<usize>,
    prepare: Cell<usize>,
    output_error: bool,
    mode: Cell<u16>,
}
struct Read {
    bytes: usize,
    storage_bytes: usize,
}
struct Writer<'a>(&'a RegularFile);
impl RegularFile {
    fn new(plan: ReadPlan, output_error: bool) -> Self {
        Self {
            plan,
            reads: Cell::new(0),
            prepare: Cell::new(0),
            output_error,
            mode: Cell::new(0o6777),
        }
    }
    fn size(&self) -> u64 {
        8192
    }
    fn id(&self) -> Option<usize> {
        None
    }
    fn begin_write(&self, _: AccessIdentity) -> Result<Writer<'_>, FileSystemError> {
        Ok(Writer(self))
    }
    fn read(&self, _: u64, bytes: &mut [u8]) -> Result<Read, FileSystemError> {
        let call = self.reads.get();
        self.reads.set(call + 1);
        match self.plan {
            ReadPlan::Error => Err(FileSystemError::Io),
            ReadPlan::SecondError if call > 0 => Err(FileSystemError::Io),
            ReadPlan::Eof => Ok(Read {
                bytes: 0,
                storage_bytes: 0,
            }),
            _ => {
                bytes.fill(7);
                Ok(Read {
                    bytes: bytes.len(),
                    storage_bytes: bytes.len(),
                })
            }
        }
    }
}
impl Writer<'_> {
    fn prepare_content_write(&self) -> Result<(), FileSystemError> {
        self.0.prepare.set(self.0.prepare.get() + 1);
        self.0.mode.set(0o0777);
        Ok(())
    }
    fn write(&self, _: u64, bytes: &[u8]) -> Result<usize, FileSystemError> {
        if self.0.output_error {
            Err(FileSystemError::Io)
        } else {
            Ok(bytes.len())
        }
    }
}
#[path = "../../../kernel/src/syscall/fs/io/sendfile/regular_transfer.rs"]
mod regular_transfer;
#[test]
fn first_input_failure_or_eof_does_not_touch_output_privileges() {
    for (plan, expected) in [(ReadPlan::Error, -5), (ReadPlan::Eof, 0)] {
        let input = RegularFile::new(plan, false);
        let output = RegularFile::new(ReadPlan::Success, false);
        let (mut source, mut target) = (0, 0);
        assert_eq!(
            regular_transfer::copy_regular_file(
                &TaskControlBlock,
                &input,
                &output,
                &mut source,
                &mut target,
                8192
            ),
            expected
        );
        assert_eq!(output.prepare.get(), 0);
        assert_eq!(output.mode.get(), 0o6777);
        assert_eq!((source, target), (0, 0));
    }
}
#[test]
fn output_failure_keeps_prepared_killpriv_without_advancing_offsets() {
    let input = RegularFile::new(ReadPlan::Success, false);
    let output = RegularFile::new(ReadPlan::Success, true);
    let (mut source, mut target) = (0, 0);
    assert_eq!(
        regular_transfer::copy_regular_file(
            &TaskControlBlock,
            &input,
            &output,
            &mut source,
            &mut target,
            8192
        ),
        -5
    );
    assert_eq!(output.prepare.get(), 1);
    assert_eq!(output.mode.get(), 0o0777);
    assert_eq!((source, target), (0, 0));
}
#[test]
fn later_input_failure_returns_only_durable_prefix_and_does_not_repeat_killpriv() {
    let input = RegularFile::new(ReadPlan::SecondError, false);
    let output = RegularFile::new(ReadPlan::Success, false);
    let (mut source, mut target) = (0, 0);
    assert_eq!(
        regular_transfer::copy_regular_file(
            &TaskControlBlock,
            &input,
            &output,
            &mut source,
            &mut target,
            8192
        ),
        4096
    );
    assert_eq!(output.prepare.get(), 1);
    assert_eq!((source, target), (4096, 4096));
}
