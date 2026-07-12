use super::{InetEndpoint, InetSocket, SocketError, stack};

/// @description NetworkStack endpoint owner 保存的标准 SOL_SOCKET policy。
#[derive(Clone, Copy, Default)]
pub(super) struct InetSocketOptions {
    /// 控制地址冲突是否允许两个 endpoint 共同 opt-in；缺失会让 daemon restart 错误 EADDRINUSE。
    pub(super) reuse_address: bool,
    /// 授权 UDP broadcast；缺失会让 DHCP 等标准 broadcast send 绕过 EACCES policy。
    pub(super) broadcast: bool,
    /// 单 interface scope 只需记录 bind/unbind；复制名称会制造第二份 interface identity。
    /// 缺失该状态会让 SO_BINDTODEVICE 成为虚假成功，并允许后续多 NIC 路由绕过 binding。
    pub(super) bound_to_device: bool,
    /// TCP_NODELAY 关闭 Nagle；缺失会让 TLS/interactive stream 的标准 latency policy 被虚假接受。
    pub(super) no_delay: bool,
}

impl InetSocket {
    /// @description 设置 Linux `SO_REUSEADDR` 并由 endpoint owner 参与 bind collision policy。
    /// @param enabled 非零 option value 的布尔投影。
    /// @return endpoint 存在时返回 unit。
    /// @errors endpoint 已被删除时返回 NotConnected。
    pub(in crate::socket) fn set_reuse_address(&self, enabled: bool) -> Result<(), SocketError> {
        let mut network = stack()?.lock();
        match self.endpoint {
            InetEndpoint::Udp(handle) => {
                network
                    .endpoints
                    .get_mut(&handle)
                    .ok_or(SocketError::NotConnected)?
                    .options
                    .reuse_address = enabled
            }
            InetEndpoint::Tcp(id) => {
                network
                    .tcp_endpoints
                    .get_mut(&id)
                    .ok_or(SocketError::NotConnected)?
                    .options
                    .reuse_address = enabled
            }
            InetEndpoint::Raw(_) => return Err(SocketError::OperationNotSupported),
        }
        Ok(())
    }

    /// @description 设置 UDP limited/subnet broadcast 发送授权。
    /// @param enabled 非零 option value 的布尔投影。
    /// @return UDP endpoint 存在时返回 unit。
    /// @errors TCP 返回 OperationNotSupported；endpoint 消失返回 NotConnected。
    pub(in crate::socket) fn set_broadcast(&self, enabled: bool) -> Result<(), SocketError> {
        if let InetEndpoint::Raw(handle) = self.endpoint {
            return super::raw_endpoint::set_broadcast(handle, enabled);
        }
        let handle = self.udp_handle()?;
        stack()?
            .lock()
            .endpoints
            .get_mut(&handle)
            .ok_or(SocketError::NotConnected)?
            .options
            .broadcast = enabled;
        Ok(())
    }

    /// @description 将 endpoint 绑定到当前唯一标准 interface `eth0`，空名称解除绑定。
    /// @param name NUL 已剥离的 interface name bytes。
    /// @return binding 状态提交给 endpoint owner 后返回 unit。
    /// @errors 未知 interface 返回 NoDevice；endpoint 消失返回 NotConnected。
    pub(in crate::socket) fn bind_to_device(&self, name: &[u8]) -> Result<(), SocketError> {
        if !name.is_empty() && name != b"eth0" {
            return Err(SocketError::NoDevice);
        }
        if let InetEndpoint::Raw(handle) = self.endpoint {
            return super::raw_endpoint::bind_to_device(handle, name);
        }
        let mut network = stack()?.lock();
        let options = match self.endpoint {
            InetEndpoint::Udp(handle) => {
                &mut network
                    .endpoints
                    .get_mut(&handle)
                    .ok_or(SocketError::NotConnected)?
                    .options
            }
            InetEndpoint::Tcp(id) => {
                &mut network
                    .tcp_endpoints
                    .get_mut(&id)
                    .ok_or(SocketError::NotConnected)?
                    .options
            }
            InetEndpoint::Raw(_) => unreachable!(),
        };
        options.bound_to_device = !name.is_empty();
        Ok(())
    }

    pub(in crate::socket) fn set_no_delay(&self, enabled: bool) -> Result<(), SocketError> {
        super::tcp::set_no_delay(self, enabled)
    }
}
