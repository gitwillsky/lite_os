use super::{Socket, SocketBackend, SocketError, UnixCredentials};

impl Socket {
    /// 返回 connected AF_UNIX endpoint 的 peer credentials。
    ///
    /// # Returns
    ///
    /// 连接或 socketpair 建立时捕获的 Linux `ucred`。
    ///
    /// # Errors
    ///
    /// 非 AF_UNIX 或未连接 endpoint 返回对应错误。
    pub(crate) fn peer_credentials(&self) -> Result<UnixCredentials, SocketError> {
        match &self.backend {
            SocketBackend::Unix(socket) => socket.peer_credentials(),
            _ => Err(SocketError::OperationNotSupported),
        }
    }

    /// 将 IP_PKTINFO policy 提交给 AF_INET endpoint owner。
    ///
    /// # Parameters
    ///
    /// - `enabled`: 是否为 recvmsg 生成 packet-info control message。
    ///
    /// # Returns
    ///
    /// endpoint policy 更新成功。
    ///
    /// # Errors
    ///
    /// 非 AF_INET endpoint 返回 OperationNotSupported。
    pub(crate) fn set_ipv4_packet_info(&self, enabled: bool) -> Result<(), SocketError> {
        match &self.backend {
            SocketBackend::Inet(socket) => socket.set_packet_info(enabled),
            _ => Err(SocketError::OperationNotSupported),
        }
    }

    /// 将 SO_REUSEADDR policy 提交给具体 endpoint owner。
    ///
    /// # Parameters
    ///
    /// - `enabled`: 非零 userspace option value 的布尔投影。
    ///
    /// # Returns
    ///
    /// AF_INET endpoint 成功更新返回 unit。
    ///
    /// # Errors
    ///
    /// 不支持的 domain 或失效 endpoint 返回错误。
    pub(crate) fn set_reuse_address(&self, enabled: bool) -> Result<(), SocketError> {
        match &self.backend {
            SocketBackend::Inet(socket) => socket.set_reuse_address(enabled),
            _ => Err(SocketError::OperationNotSupported),
        }
    }

    /// 将 SO_BROADCAST policy 提交给 UDP endpoint owner。
    ///
    /// # Parameters
    ///
    /// - `enabled`: 非零 userspace option value 的布尔投影。
    ///
    /// # Returns
    ///
    /// AF_INET UDP endpoint 成功更新返回 unit。
    ///
    /// # Errors
    ///
    /// 不支持的 domain/type 或失效 endpoint 返回错误。
    pub(crate) fn set_broadcast(&self, enabled: bool) -> Result<(), SocketError> {
        match &self.backend {
            SocketBackend::Inet(socket) => socket.set_broadcast(enabled),
            _ => Err(SocketError::OperationNotSupported),
        }
    }

    /// 将 SO_BINDTODEVICE interface name 提交给 endpoint owner。
    ///
    /// # Parameters
    ///
    /// - `name`: 已去除 NUL 的 interface name；空值解除绑定。
    ///
    /// # Returns
    ///
    /// 当前唯一 eth0 binding 成功更新返回 unit。
    ///
    /// # Errors
    ///
    /// 未知 interface、domain 或失效 endpoint 返回错误。
    pub(crate) fn bind_to_device(&self, name: &[u8]) -> Result<(), SocketError> {
        match &self.backend {
            SocketBackend::Inet(socket) => socket.bind_to_device(name),
            _ => Err(SocketError::OperationNotSupported),
        }
    }

    /// 设置 AF_INET raw endpoint 的 IPv4 hop limit。
    ///
    /// # Parameters
    ///
    /// - `value`: 已按 Linux `IP_TTL` 约束验证的 1..=255 值。
    ///
    /// # Returns
    ///
    /// raw ICMP endpoint 成功提交返回 unit。
    ///
    /// # Errors
    ///
    /// 非 AF_INET endpoint 或失效 endpoint 返回错误。
    pub(crate) fn set_ipv4_hop_limit(&self, value: u8) -> Result<(), SocketError> {
        match &self.backend {
            SocketBackend::Inet(socket) => socket.set_hop_limit(value),
            _ => Err(SocketError::OperationNotSupported),
        }
    }

    /// 设置 TCP_NODELAY policy。
    ///
    /// # Parameters
    ///
    /// - `enabled`: 是否禁用 Nagle aggregation。
    ///
    /// # Returns
    ///
    /// AF_INET TCP endpoint policy 更新成功。
    ///
    /// # Errors
    ///
    /// 非 AF_INET/TCP endpoint 返回对应错误。
    pub(crate) fn set_tcp_no_delay(&self, enabled: bool) -> Result<(), SocketError> {
        match &self.backend {
            SocketBackend::Inet(socket) => socket.set_no_delay(enabled),
            _ => Err(SocketError::OperationNotSupported),
        }
    }

    /// 查询 recvmsg 是否应生成 IP_PKTINFO。
    ///
    /// # Returns
    ///
    /// AF_INET endpoint 的当前 policy；其他 domain 为 false。
    pub(crate) fn ipv4_packet_info(&self) -> bool {
        matches!(&self.backend, SocketBackend::Inet(socket) if socket.packet_info())
    }
}
