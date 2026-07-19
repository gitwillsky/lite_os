use alloc::vec::Vec;

use smoltcp::{
    iface::SocketHandle,
    socket::tcp::{self, CongestionControl},
};

use crate::socket::SocketError;

use super::NetworkStack;

const TCP_BUFFER_BYTES: usize = 32 * 1024;

fn allocate_buffer() -> Result<Vec<u8>, SocketError> {
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(TCP_BUFFER_BYTES)
        .map_err(|_| SocketError::NoMemory)?;
    bytes.resize(TCP_BUFFER_BYTES, 0);
    Ok(bytes)
}

pub(super) fn placeholder_socket() -> tcp::Socket<'static> {
    tcp::Socket::new(
        tcp::SocketBuffer::new(Vec::new()),
        tcp::SocketBuffer::new(Vec::new()),
    )
}

pub(super) fn add_socket(network: &mut NetworkStack) -> Result<SocketHandle, SocketError> {
    let mut socket = tcp::Socket::new(
        tcp::SocketBuffer::new(allocate_buffer()?),
        tcp::SocketBuffer::new(allocate_buffer()?),
    );
    // Reno 不使用 kernel FPU context，且比关闭 congestion control 更符合共享网络语义。
    socket.set_congestion_control(CongestionControl::Reno);
    network.add_socket(socket)
}
