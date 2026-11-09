//! 独立只读跟踪能力：查询集合可替换，Event 合并通知，快照使用一个只读 VMO。
use super::*;
use native_runtime::{Event, Vmo};
use router_protocol::tracking::{self, Query};
const SNAPSHOT: u32 = 1;
const UPDATE: u32 = 2;
const VERSION: u32 = 1;

fn status(error: tracking::Error) -> Status {
    match error {
        tracking::Error::Invalid => native_abi::ERR_INVALID_ARGS,
        tracking::Error::NoMemory => native_abi::ERR_NO_MEMORY,
    }
}
pub(super) fn queries(
    request: &mut Reader<'_>,
    mut handles: Vec<Handle>,
) -> Result<Vec<Query>, Status> {
    if request.u32()? != VERSION {
        return Err(native_abi::ERR_INVALID_ARGS);
    }
    let length = request.u64()?;
    request.finish()?;
    if handles.len() != 1 {
        return Err(native_abi::ERR_INVALID_ARGS);
    }
    let bytes = Vmo::from_handle(service_rpc::take_handle(&mut handles, 0)?).read_bytes(length)?;
    tracking::decode_queries(&bytes).map_err(status)
}
pub(super) fn request(ordinal: u32, queries: &[Query]) -> Result<(Writer, Vec<Handle>), Status> {
    let bytes = tracking::encode_queries(queries).map_err(status)?;
    let content = Vmo::from_bytes(&bytes)?;
    let mut request = Writer::request(ordinal);
    request.try_reserve(12)?;
    request.u32(VERSION).u64(bytes.len() as u64);
    let mut handles = Vec::new();
    handles
        .try_reserve_exact(1)
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    handles.push(content.into_handle());
    Ok((request, handles))
}
/// 跟踪会话的服务端；不持来源/FIB 写入接口，关闭 channel 退休订阅。
pub trait TrackingService: Send + Sync {
    /// 读取同一已确认代际的完整有序结果。
    /// # Errors
    /// 初始化未确认、会话终止、容量/资源不足或非法结果。
    fn snapshot(&self) -> Result<Vec<u8>, Status>;
    /// 原子替换本会话的观察集合；不是路由来源更新。
    /// # Errors
    /// 非法查询、已关闭会话或资源不足；失败保留原集合。
    fn update(&self, queries: Vec<Query>) -> Result<(), Status>;
}
/// 分派严格的快照/观察集合更新请求；无副作用前完整检查输入。
/// # Errors
/// 操作未知、framing/句柄非法、会话终止或资源不足。
pub fn dispatch_tracking(
    service: &dyn TrackingService,
    ordinal: u32,
    request: &mut Reader<'_>,
    handles: Vec<Handle>,
    reply: &mut Writer,
) -> Result<Vec<Handle>, Status> {
    match ordinal {
        SNAPSHOT => {
            request.finish()?;
            if !handles.is_empty() {
                return Err(native_abi::ERR_INVALID_ARGS);
            }
            reply.try_reserve(8)?;
            let mut returned = Vec::new();
            returned
                .try_reserve_exact(1)
                .map_err(|_| native_abi::ERR_NO_MEMORY)?;
            let bytes = service.snapshot()?;
            let content = Vmo::from_bytes(&bytes)?;
            reply.u64(bytes.len() as u64);
            returned.push(content.into_handle());
            Ok(returned)
        }
        UPDATE => {
            service.update(queries(request, handles)?)?;
            Ok(Vec::new())
        }
        _ => Err(native_abi::ERR_NOT_SUPPORTED),
    }
}
pub(super) fn returned(
    open: impl FnOnce() -> Result<(Channel, Event), Status>,
) -> Result<Vec<Handle>, Status> {
    let mut handles = Vec::new();
    handles
        .try_reserve_exact(2)
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    let session = open()?;
    handles.push(session.0.into_handle());
    handles.push(session.1.into_handle());
    Ok(handles)
}
/// 一个授权范围内的独立跟踪会话；Event 只表示要重新取快照，必须同时观察 channel 的 PEER_CLOSED。
pub struct Tracking {
    channel: Channel,
    changes: Event,
}
impl Tracking {
    pub(super) fn from_reply(mut reply: service_rpc::Reply) -> Result<Self, Status> {
        reply.reader().finish()?;
        if reply.handles.len() != 2 {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        let channel = Channel::from_handle(service_rpc::take_handle(&mut reply.handles, 0)?);
        let changes = Event::from_handle(service_rpc::take_handle(&mut reply.handles, 0)?);
        Ok(Self { channel, changes })
    }
    /// 此 RIB 实例退出/跟踪终止后为 PEER_CLOSED，旧代际不跨实例复用。
    pub fn channel(&self) -> &Channel {
        &self.channel
    }
    /// clear → arm → snapshot；通知可合并，不能把 Event 当作变化历史。
    pub fn changes(&self) -> &Event {
        &self.changes
    }
    /// 原子替换有序观察集合，支持空集合，失败不丢原观察。
    /// # Errors
    /// 查询非法、会话关闭、格式错误或资源不足。
    pub fn update(&self, queries: &[Query]) -> Result<(), Status> {
        let (request, handles) = request(UPDATE, queries)?;
        let reply = service_rpc::call(&self.channel, &request, handles)?;
        reply.reader().finish()?;
        if !reply.handles.is_empty() {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        Ok(())
    }
    /// 捕获并一次完整校验快照；返回对象保活正文，通过 view 借用读取，不重复扫描。
    /// # Errors
    /// 初始化未确认、会话关闭、格式错误或资源不足；不交出部分结果。
    pub fn snapshot(&self) -> Result<tracking::OwnedSnapshot, Status> {
        let mut reply = service_rpc::call(&self.channel, &Writer::request(SNAPSHOT), Vec::new())?;
        let mut reader = reply.reader();
        let length = reader.u64()?;
        reader.finish()?;
        if reply.handles.len() != 1 {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        let bytes = Vmo::from_handle(service_rpc::take_handle(&mut reply.handles, 0)?)
            .read_bytes(length)?;
        tracking::OwnedSnapshot::capture(bytes).map_err(status)
    }
}
