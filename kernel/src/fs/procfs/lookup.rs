use super::{
    DirectoryEntry, FileSystemError, InodeType, ProcProcessSnapshot, ProcSnapshot,
    ProcThreadSnapshot,
};

pub(super) fn directory_entry(inode: u64, kind: InodeType, name: &[u8]) -> DirectoryEntry {
    DirectoryEntry {
        inode,
        kind,
        name: name.to_vec(),
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
