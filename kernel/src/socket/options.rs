use super::{
    Socket, SocketBackend, SocketError, SocketIoDirection, SocketTimeout, UnixCredentials,
};

impl Socket {
    /// 已创建 socket 的实际网络域；accept 从 listener 捕获，setns 不重定向既有 fd。
    pub(crate) fn net_namespace(&self) -> alloc::sync::Arc<super::NetNamespace> {
        self.memory.namespace()
    }
    pub(crate) fn reserve_control_memory(
        &self,
        bytes: usize,
    ) -> Option<super::ControlMemoryCharge> {
        self.memory.reserve(bytes)
    }
    /// 更新实际支持 domain 的 buffer policy；未接入的 backend 不能只改变 getter。
    pub(crate) fn set_buffer_limit(
        &self,
        direction: SocketIoDirection,
        value: i32,
        forced: bool,
    ) -> Result<(), SocketError> {
        match &self.backend {
            SocketBackend::Unix(socket) => {
                socket.set_buffer_limit(direction, value, forced);
                Ok(())
            }
            SocketBackend::Kobject(socket) => socket.set_buffer_limit(direction, value, forced),
            SocketBackend::Route(socket) => socket.set_buffer_limit(direction, value, forced),
            _ => Err(SocketError::OperationNotSupported),
        }
    }
    /// 投影 backend 实际限额；与实际 admission/退款共用 owner。
    pub(crate) fn buffer_limit(&self, direction: SocketIoDirection) -> Result<i32, SocketError> {
        match &self.backend {
            SocketBackend::Unix(socket) => Ok(socket.buffer_limit(direction)),
            SocketBackend::Kobject(socket) => socket.buffer_limit(direction),
            SocketBackend::Route(socket) => socket.buffer_limit(direction),
            _ => Err(SocketError::OperationNotSupported),
        }
    }

    /// 设置已由 syscall codec 验证的单方向超时；既有等待不重新计时。
    pub(crate) fn set_timeout(
        &self,
        direction: SocketIoDirection,
        value: SocketTimeout,
    ) -> Result<(), SocketError> {
        if let SocketBackend::Inet(socket) = &self.backend {
            socket.set_timeout(direction, value)
        } else {
            self.timeouts.set(direction, value);
            Ok(())
        }
    }
    /// 从实际 socket policy 冻结一次阻塞操作的超时。
    pub(crate) fn timeout(&self, direction: SocketIoDirection) -> SocketTimeout {
        self.timeouts.get(direction)
    }

    /// 更新实际 AF_UNIX endpoint 的消息凭据 policy；不创建 peer credential 镜像。
    pub(crate) fn set_pass_credentials(&self, enabled: bool) -> Result<(), SocketError> {
        match &self.backend {
            SocketBackend::Unix(socket) => {
                socket.set_pass_credentials(enabled);
                Ok(())
            }
            _ => Err(SocketError::OperationNotSupported),
        }
    }
    /// 查询实际 endpoint policy；其他协议不生成 AF_UNIX credentials。
    pub(crate) fn pass_credentials(&self) -> bool {
        match &self.backend {
            SocketBackend::Unix(socket) => socket.pass_credentials(),
            _ => false,
        }
    }

    /// 返回 AF_UNIX 实际 peer/listener credential owner 的快照。
    ///
    /// # Returns
    ///
    /// listen/connect/socketpair 捕获的 Linux `ucred`；其他 socket 或未安装时返回 None。
    pub(crate) fn peer_credentials(&self) -> Option<UnixCredentials> {
        match &self.backend {
            SocketBackend::Unix(socket) => socket.peer_credentials(),
            _ => None,
        }
    }

    /// 配置实际 RAW endpoint 的 IPv4 header 输入模式。
    ///
    /// # Parameters
    ///
    /// - `enabled`: 输入是否包含完整 IPv4 header。
    ///
    /// # Errors
    ///
    /// 非 RAW endpoint 返回 OperationNotSupported。
    pub(crate) fn set_ipv4_header_included(&self, enabled: bool) -> Result<(), SocketError> {
        match &self.backend {
            SocketBackend::Inet(socket) => socket.set_header_included(enabled),
            _ => Err(SocketError::OperationNotSupported),
        }
    }

    /// 将 IP_PKTINFO policy 提交给 AF_INET endpoint owner。
    ///
    /// # Parameters
    ///
    /// - `enabled`: 是否为 recvmsg 生成 packet-info control message。
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
    pub(crate) fn bind_to_device(
        &self,
        name: &[u8],
        authority: &crate::namespace::NamespaceAuthority,
    ) -> Result<(), SocketError> {
        match &self.backend {
            SocketBackend::Inet(socket) => socket.bind_to_device(name, authority),
            _ => Err(SocketError::OperationNotSupported),
        }
    }

    /// 设置 AF_INET endpoint 的 IPv4 hop limit。
    ///
    /// # Parameters
    ///
    /// - `value`: 已验证的 1..=255 值；None 恢复 namespace 默认。
    ///
    /// # Returns
    ///
    /// TCP/UDP/raw endpoint 成功提交返回 unit。
    ///
    /// # Errors
    ///
    /// 非 AF_INET endpoint 或失效 endpoint 返回错误。
    pub(crate) fn set_ipv4_hop_limit(&self, value: Option<u8>) -> Result<(), SocketError> {
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

    /// 将原生keepalive scalar投递给实际INET owner，不在facade保存选项镜像。
    pub(crate) fn set_internet_timer(
        &self,
        option: super::InternetOption,
        value: i32,
    ) -> Result<(), SocketError> {
        match &self.backend {
            SocketBackend::Inet(socket) => socket.set_timer_option(option, value),
            _ => Err(SocketError::OperationNotSupported),
        }
    }

    /// TCP_MAXSEG 委托实际 TCP owner；不对其他 protocol 保存虚假 getter 状态。
    pub(crate) fn set_tcp_max_segment(&self, value: i32) -> Result<(), SocketError> {
        match &self.backend {
            SocketBackend::Inet(socket) => socket.set_max_segment(value),
            _ => Err(SocketError::OperationNotSupported),
        }
    }

    /// 查询实际 INET owner 的已实现选项；不创建只改变 getter 的 façade 状态。
    /// # Errors
    /// 非 INET 或不适用于该 protocol 的选项返回标准 domain 错误。
    pub(crate) fn internet_option(
        &self,
        option: super::InternetOption,
    ) -> Result<i32, SocketError> {
        match &self.backend {
            SocketBackend::Inet(socket) => socket.option(option),
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
