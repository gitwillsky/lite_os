use alloc::vec::Vec;

use super::{FileSystemError, FileSystemStatistics};

pub(super) fn write_mount_record(
    output: &mut Vec<u8>,
    source: &[u8],
    target: &[u8],
    statistics: &FileSystemStatistics,
) -> Result<(), FileSystemError> {
    let escaped_fields = source
        .len()
        .checked_add(target.len())
        .and_then(|length| length.checked_mul(4))
        .ok_or(FileSystemError::OutOfMemory)?;
    let required = escaped_fields
        .checked_add(statistics.type_name.len())
        .and_then(|length| length.checked_add(16))
        .ok_or(FileSystemError::OutOfMemory)?;
    output
        .try_reserve(required)
        .map_err(|_| FileSystemError::OutOfMemory)?;
    write_field(output, source);
    output.push(b' ');
    write_field(output, target);
    output.push(b' ');
    output.extend_from_slice(statistics.type_name.as_bytes());
    output.extend_from_slice(if statistics.flags & 1 != 0 {
        b" ro 0 0\n"
    } else {
        b" rw 0 0\n"
    });
    Ok(())
}

fn write_field(output: &mut Vec<u8>, field: &[u8]) {
    for byte in field {
        match byte {
            b' ' => output.extend_from_slice(b"\\040"),
            b'\t' => output.extend_from_slice(b"\\011"),
            b'\n' => output.extend_from_slice(b"\\012"),
            b'\\' => output.extend_from_slice(b"\\134"),
            byte => output.push(*byte),
        }
    }
}
