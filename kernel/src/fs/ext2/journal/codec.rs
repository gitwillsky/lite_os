use super::*;

pub(super) fn be16(bytes: &[u8], offset: usize) -> Result<u16, FileSystemError> {
    let raw = bytes
        .get(offset..offset + 2)
        .ok_or(FileSystemError::InvalidFileSystem)?;
    Ok(u16::from_be_bytes([raw[0], raw[1]]))
}

pub(super) fn be32(bytes: &[u8], offset: usize) -> Result<u32, FileSystemError> {
    let raw = bytes
        .get(offset..offset + 4)
        .ok_or(FileSystemError::InvalidFileSystem)?;
    Ok(u32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]]))
}

pub(super) fn put_be16(bytes: &mut [u8], offset: usize, value: u16) -> Result<(), FileSystemError> {
    bytes
        .get_mut(offset..offset + 2)
        .ok_or(FileSystemError::InvalidFileSystem)?
        .copy_from_slice(&value.to_be_bytes());
    Ok(())
}

pub(super) fn put_be32(bytes: &mut [u8], offset: usize, value: u32) -> Result<(), FileSystemError> {
    bytes
        .get_mut(offset..offset + 4)
        .ok_or(FileSystemError::InvalidFileSystem)?
        .copy_from_slice(&value.to_be_bytes());
    Ok(())
}

pub(super) fn put_header(
    bytes: &mut [u8],
    kind: u32,
    sequence: u32,
) -> Result<(), FileSystemError> {
    put_be32(bytes, 0, JBD2_MAGIC)?;
    put_be32(bytes, 4, kind)?;
    put_be32(bytes, 8, sequence)
}
