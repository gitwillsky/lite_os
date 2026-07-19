use super::{FileSystemError, ProcProcessSnapshot, ProcSnapshot, ProcThreadSnapshot};

pub(super) fn decimal_name(value: usize, output: &mut [u8; 20]) -> &[u8] {
    let mut value = value;
    let mut start = output.len();
    loop {
        start -= 1;
        output[start] = b'0' + (value % 10) as u8;
        value /= 10;
        if value == 0 {
            return &output[start..];
        }
    }
}

pub(super) fn find_process(
    snapshot: &ProcSnapshot,
    pid: usize,
) -> Result<&ProcProcessSnapshot, FileSystemError> {
    snapshot
        .processes
        .iter()
        .find(|process| process.pid == pid)
        .ok_or(FileSystemError::NotFound)
}

pub(super) fn find_thread(
    process: &ProcProcessSnapshot,
    tid: usize,
) -> Result<&ProcThreadSnapshot, FileSystemError> {
    process
        .threads
        .iter()
        .find(|thread| thread.tid == tid)
        .ok_or(FileSystemError::NotFound)
}

pub(super) fn parse_pid(name: &[u8]) -> Option<usize> {
    if name.is_empty() || name.iter().any(|byte| !byte.is_ascii_digit()) {
        return None;
    }
    name.iter().try_fold(0usize, |pid, byte| {
        pid.checked_mul(10)?.checked_add((byte - b'0') as usize)
    })
}
