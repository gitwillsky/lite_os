//! Packet socket 的控制接口与分派；L3 INET 与其选项归各自领域。
use super::*;

pub(super) const STATISTICS: u32 = 22;
pub(super) const RECEIVE_RING: u32 = 23;
mod ring;
pub use ring::{CaptureRing, FrameRingLayout};

/// 服务端：一个 packet socket；全部操作不阻塞。
pub trait PacketSocketService: Send + Sync {
    fn bind(&self, address: PacketAddress) -> Result<(), SocketError>;
    fn address(&self) -> Result<PacketAddress, SocketError>;
    fn send_to(&self, input: &[u8], target: Option<PacketAddress>) -> Result<usize, SocketError>;
    fn receive(&self, limit: usize, peek: bool) -> Result<PacketReceived, SocketError>;
    fn take_error(&self) -> Option<SocketError>;
    /// 原子读取并清零自上次读取以来的匹配数与其中的丢弃数。
    /// # Errors
    /// 已关闭的 packet endpoint。
    fn take_statistics(&self) -> Result<[u32; 2], SocketError>;
    /// 安装已准备的接收环，或恢复普通接收队列。
    /// # Errors
    /// endpoint 已关闭、环正在使用或链路头不匹配。
    fn set_receive_ring(&self, ring: Option<CaptureRing>) -> Result<(), SocketError>;
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
    mut handles: Vec<Handle>,
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
        STATISTICS => {
            if !handles.is_empty() {
                return Err(native_abi::ERR_INVALID_ARGS);
            }
            request.finish()?;
            reply.try_reserve(9)?;
            if let Some([packets, drops]) = put_result(reply, service.take_statistics()) {
                reply.u32(packets).u32(drops);
            }
        }
        RECEIVE_RING => {
            reply.try_reserve(1)?;
            let version = request.u32()?;
            if version != FrameRingLayout::VERSION {
                return Err(native_abi::ERR_INVALID_ARGS);
            }
            let ring = if request.bool()? {
                let layout = FrameRingLayout::read(request)?;
                request.finish()?;
                if handles.len() != 1 {
                    return Err(native_abi::ERR_INVALID_ARGS);
                }
                let vmo = Vmo::from_handle(take_handle(&mut handles, 0)?);
                match CaptureRing::new(vmo, layout) {
                    Ok(ring) => Some(ring),
                    Err(error) => {
                        put_result(reply, Err::<(), _>(error));
                        return Ok(Vec::new());
                    }
                }
            } else {
                request.finish()?;
                if !handles.is_empty() {
                    return Err(native_abi::ERR_INVALID_ARGS);
                }
                None
            };
            put_result(reply, service.set_receive_ring(ring));
        }
        ordinal::CLOSE => {
            service.close();
            reply.u8(0);
        }
        _ => return Err(native_abi::ERR_NOT_SUPPORTED),
    }
    Ok(Vec::new())
}
