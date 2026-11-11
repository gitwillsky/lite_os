//! TCP 流存储由调用者完整准备；未打开/临时借出时只留同一 endpoint 内的空机制。
use super::{SocketError, transport};
use alloc::vec::Vec;
const TCP_BUFFER_BYTES: usize = 32 * 1024;
fn allocate_buffer() -> Result<Vec<u8>, SocketError> {
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(TCP_BUFFER_BYTES)
        .map_err(|_| SocketError::NoMemory)?;
    bytes.resize(TCP_BUFFER_BYTES, 0);
    Ok(bytes)
}
pub(super) fn placeholder_socket() -> transport::Socket<'static> {
    transport::Socket::new(
        transport::SocketBuffer::new(Vec::new()),
        transport::SocketBuffer::new(Vec::new()),
    )
}
pub(super) fn allocate_socket() -> Result<transport::Socket<'static>, SocketError> {
    Ok(transport::Socket::new(
        transport::SocketBuffer::new(allocate_buffer()?),
        transport::SocketBuffer::new(allocate_buffer()?),
    ))
}
