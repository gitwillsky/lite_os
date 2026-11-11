//! INET 控制接口与请求分派；数据队列及应用编号不属于此层。
use super::*;

/// 服务端：一个 INET socket；全部操作不阻塞。
pub trait InetSocketService: Send + Sync {
    fn bind(&self, address: InetAddress) -> Result<(), SocketError>;
    fn connect(&self, peer: InetAddress) -> Result<(), SocketError>;
    fn disconnect(&self) -> Result<(), SocketError>;
    fn address(&self) -> Result<InetAddress, SocketError>;
    fn peer_address(&self) -> Result<InetAddress, SocketError>;
    fn send_to(
        &self,
        input: &[u8],
        target: Option<InetAddress>,
        packet_info: &[InternetSendPacketInfo],
    ) -> Result<usize, SocketError>;
    fn receive(&self, limit: usize, peek: bool) -> Result<InetReceived, SocketError>;
    /// 消费一条发送错误。空队列返回 Again，流式端点返回 OperationNotSupported。
    fn receive_error(&self, limit: usize) -> Result<InetErrorReceived, SocketError>;
    fn is_listening(&self) -> Result<bool, SocketError>;
    fn listen(&self, backlog: usize) -> Result<(), SocketError>;
    fn accept(&self) -> Result<SocketEndpoint, SocketError>;
    fn connection_result(&self) -> Result<(), SocketError>;
    fn take_error(&self) -> Option<SocketError>;
    fn shutdown(&self, how: usize) -> Result<(), SocketError>;
    fn poll_state(&self) -> SocketPollState;
    fn set_option(&self, change: InetOptionChange) -> Result<(), SocketError>;
    /// Add/replace a peer key; an empty key deletes the exact selector.
    /// # Errors
    /// Invalid selector, missing deletion target, resource failure or unsupported protocol.
    fn set_md5_key(&self, selector: TcpMd5Selector, key: &[u8]) -> Result<(), SocketError>;
    /// `privileged`：调用者在域的 owning user namespace 中具有 `CAP_NET_RAW`（已绑定后改绑需要它）。
    /// 读取 ICMP 接收位图，或按 mask 原子更新选定位；不修改已入队报文。
    /// # Parameters
    /// update 为 None 时只读；Some 时只修改掩码选中的类型。
    /// # Returns
    /// 更新后的完整八字位图；IPv4 ICMP 只使用第一个字。
    /// # Errors
    /// 非 ICMP RAW、非法过滤范围、资源或已关闭 endpoint。
    fn icmp_filter(&self, update: Option<IcmpFilterUpdate>) -> Result<[u32; 8], SocketError>;
    fn bind_to_device(&self, name: &[u8], privileged: bool) -> Result<(), SocketError>;
    fn option(&self, option: InternetOption) -> Result<i32, SocketError>;
    /// 使用者关闭了 socket（CLOSE 或 channel 对端关闭）；重复调用无副作用。
    fn close(&self);
}

/// 分派 INET socket channel 的一条请求。
///
/// # Errors
///
/// 未知请求或格式错误；服务的领域失败编码在回复中。
pub fn dispatch_inet(
    service: &dyn InetSocketService,
    ordinal: u32,
    request: &mut Reader<'_>,
    handles: Vec<Handle>,
    reply: &mut Writer,
) -> Result<Vec<Handle>, Status> {
    let mut reply_handles = Vec::new();
    match ordinal {
        ordinal::BIND => {
            put_result(reply, service.bind(get_inet(request)?));
        }
        ordinal::CONNECT => {
            put_result(reply, service.connect(get_inet(request)?));
        }
        ordinal::DISCONNECT => {
            put_result(reply, service.disconnect());
        }
        ordinal::ADDRESS | ordinal::PEER_ADDRESS => {
            let result = if ordinal == ordinal::ADDRESS {
                service.address()
            } else {
                service.peer_address()
            };
            if let Some(address) = put_result(reply, result) {
                put_inet(reply, address);
            }
        }
        ordinal::IS_LISTENING => {
            if let Some(listening) = put_result(reply, service.is_listening()) {
                reply.bool(listening);
            }
        }
        ordinal::LISTEN => {
            put_result(reply, service.listen(request.u64()? as usize));
        }
        ordinal::ACCEPT => {
            if let Some(endpoint) = put_result(reply, service.accept()) {
                reply_handles = endpoint_handles(endpoint);
            }
        }
        ordinal::CONNECTION_RESULT => {
            put_result(reply, service.connection_result());
        }
        ordinal::TAKE_ERROR => {
            reply.u8(service.take_error().map_or(0, SocketError::code));
        }
        ordinal::SHUTDOWN => {
            put_result(reply, service.shutdown(request.u64()? as usize));
        }
        ordinal::SET_OPTION => {
            if !handles.is_empty() {
                return Err(native_abi::ERR_INVALID_ARGS);
            }
            let change = get_option_change(request)?;
            request.finish()?;
            reply.try_reserve(1)?;
            put_result(reply, service.set_option(change));
        }
        ordinal::ICMP_FILTER => {
            if !handles.is_empty() {
                return Err(native_abi::ERR_INVALID_ARGS);
            }
            options::dispatch_icmp_filter(service, request, reply)?;
        }
        ordinal::MD5_KEY => {
            if !handles.is_empty() {
                return Err(native_abi::ERR_INVALID_ARGS);
            }
            options::dispatch_md5(service, request, reply)?;
        }
        ordinal::BIND_TO_DEVICE => {
            let name = request.bytes()?;
            put_result(reply, service.bind_to_device(name, request.bool()?));
        }
        ordinal::OPTION => {
            let option =
                InternetOption::from_code(request.u8()?).ok_or(native_abi::ERR_INVALID_ARGS)?;
            if let Some(value) = put_result(reply, service.option(option)) {
                reply.i32(value);
            }
        }
        ordinal::CLOSE => {
            service.close();
            reply.u8(0);
        }
        _ => return Err(native_abi::ERR_NOT_SUPPORTED),
    }
    Ok(reply_handles)
}
