//! Packet socket 的控制接口与分派；L3 INET 与其选项归各自领域。
use super::*;

/// 服务端：一个 packet socket；全部操作不阻塞。
pub trait PacketSocketService: Send + Sync {
    fn bind(&self, address: PacketAddress) -> Result<(), SocketError>;
    fn address(&self) -> Result<PacketAddress, SocketError>;
    fn send_to(&self, input: &[u8], target: Option<PacketAddress>) -> Result<usize, SocketError>;
    fn receive(&self, limit: usize, peek: bool) -> Result<PacketReceived, SocketError>;
    fn take_error(&self) -> Option<SocketError>;
    fn poll_state(&self) -> SocketPollState;
    /// 使用者关闭了 socket（CLOSE 或 channel 对端关闭）；重复调用无副作用。
    fn close(&self);
}

/// 分派 packet socket channel 的一条请求。
///
/// # Errors
///
/// 未知请求或格式错误；服务的领域失败编码在回复中。
pub fn dispatch_packet(
    service: &dyn PacketSocketService,
    ordinal: u32,
    request: &mut Reader<'_>,
    _handles: Vec<Handle>,
    reply: &mut Writer,
) -> Result<Vec<Handle>, Status> {
    match ordinal {
        ordinal::BIND => {
            put_result(reply, service.bind(get_packet_address(request)?));
        }
        ordinal::ADDRESS => {
            if let Some(address) = put_result(reply, service.address()) {
                put_packet_address(reply, address);
            }
        }
        ordinal::TAKE_ERROR => {
            reply.u8(service.take_error().map_or(0, SocketError::code));
        }
        ordinal::CLOSE => {
            service.close();
            reply.u8(0);
        }
        _ => return Err(native_abi::ERR_NOT_SUPPORTED),
    }
    Ok(Vec::new())
}
