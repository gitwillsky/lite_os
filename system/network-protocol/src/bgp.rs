//! BGP 原生会话能力；只有配置 owner 获得工厂，关闭实例能力即停止该域。
use crate::routing::RoutingSource;
use alloc::vec::Vec;
use core::num::NonZeroU32;
use native_runtime::{
    Channel, Handle, Vmo,
    native_abi::{self, Status},
};
use router_protocol::bgp::{self, Configuration, Snapshot};
use service_rpc::{Reader, Writer};
/// 工厂创建完整会话域；版本/正文长度及一个配置 VMO。
pub const OPEN: u32 = 1;
/// 实例查询同一 actor 的状态；版本参数，无输入句柄。
pub const SNAPSHOT: u32 = 1;
/// 按规范前缀查询已接收候选及最近确认的协议判定；版本/正文长度和一个查询 VMO。
pub const ROUTES: u32 = 2;
/// 为当前连接登记单族刷新请求；版本、正文长度和一个请求 VMO。
pub const REFRESH: u32 = 3;

/// 配置 owner 持有的 BGP 域工厂，不包含应用可见 syscall/ioctl。
pub struct BgpConfigurator(Channel);
impl BgpConfigurator {
    /// 接管由服务路由目录明确授予的工厂连接。
    pub fn new(channel: Channel) -> Self {
        Self(channel)
    }
    /// 准备完整会话域；首次状态握手确认原生 actor 初始化成功后才返回。
    /// configuration 是已验证配置；source 移交独立来源能力，minimum 必须来自该能力的实际授权。
    /// 返回实例寿命/观察能力；授权最低偏好只是显式元数据，不能扩大 source 的权限。
    /// # Errors
    /// 配置非法、资源不足、网络初始化或传输失败；失败关闭临时实例能力。
    pub fn start(
        &self,
        configuration: &Configuration,
        source: RoutingSource,
        minimum: NonZeroU32,
    ) -> Result<BgpInstance, Status> {
        let bytes = configuration
            .encode()
            .map_err(|_| native_abi::ERR_INVALID_ARGS)?;
        let content = Vmo::from_bytes(&bytes)?;
        let mut request = Writer::request(OPEN);
        request.try_reserve(16)?;
        request
            .u32(bgp::VERSION)
            .u32(minimum.get())
            .u64(bytes.len() as u64);
        let mut handles = Vec::new();
        handles
            .try_reserve_exact(2)
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        handles.push(content.into_handle());
        handles.push(source.into_channel().into_handle());
        let mut reply = service_rpc::call(&self.0, &request, handles)?;
        reply.reader().finish()?;
        if reply.handles.len() != 1 {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        let instance = BgpInstance(Channel::from_handle(service_rpc::take_handle(
            &mut reply.handles,
            0,
        )?));
        instance.snapshot()?;
        Ok(instance)
    }
}

/// 单个 BGP 域的寿命与观察能力；Drop 停止连接，没有持久配置副本。
pub struct BgpInstance(Channel);
impl BgpInstance {
    /// 供配置 owner 观察原生实例退出。
    pub fn channel(&self) -> &Channel {
        &self.0
    }
    /// 查询同一 actor 当前的会话与接收路由数量。
    /// # Errors
    /// 实例已退出、资源不足或回复畸形。
    pub fn snapshot(&self) -> Result<Snapshot, Status> {
        let mut request = Writer::request(SNAPSHOT);
        request.try_reserve(4)?;
        request.u32(bgp::VERSION);
        let bytes = Self::document(service_rpc::call(&self.0, &request, Vec::new())?)?;
        Snapshot::parse(&bytes).map_err(|_| native_abi::ERR_INVALID_ARGS)
    }
    /// 查询同一原生 actor 的单前缀结果，不派生安装权限。
    /// # Errors
    /// 非法前缀、实例退出、资源不足或回复非法。
    pub fn routes(
        &self,
        query: router_protocol::tracking::IpPrefix,
    ) -> Result<bgp::Routes, Status> {
        let bytes = query.encode().map_err(|_| native_abi::ERR_INVALID_ARGS)?;
        let bytes = Self::document(self.with_document(ROUTES, &bytes)?)?;
        let routes = bgp::Routes::parse(&bytes).map_err(|_| native_abi::ERR_INVALID_ARGS)?;
        if routes.query != query {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        Ok(routes)
    }
    /// 让当前 BGP 会话请求重发。成功确认排队，送达与选路结果由协议推进。
    /// # Errors
    /// 邻居不存在、会话未建立、未协商所需能力、实例已退出或资源不足。
    pub fn request_refresh(&self, query: &bgp::RefreshRequest) -> Result<(), Status> {
        let bytes = query.encode().map_err(|_| native_abi::ERR_INVALID_ARGS)?;
        let reply = self.with_document(REFRESH, &bytes)?;
        reply.reader().finish()?;
        if !reply.handles.is_empty() {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        Ok(())
    }
    fn with_document(&self, operation: u32, bytes: &[u8]) -> Result<service_rpc::Reply, Status> {
        let content = Vmo::from_bytes(bytes)?;
        let mut request = Writer::request(operation);
        request.try_reserve(12)?;
        request.u32(bgp::VERSION).u64(bytes.len() as u64);
        let mut handles = Vec::new();
        handles
            .try_reserve_exact(1)
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        handles.push(content.into_handle());
        service_rpc::call(&self.0, &request, handles)
    }
    fn document(mut reply: service_rpc::Reply) -> Result<Vec<u8>, Status> {
        let mut reader = reply.reader();
        let length = reader.u64()?;
        reader.finish()?;
        if reply.handles.len() != 1 {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        Vmo::from_handle(service_rpc::take_handle(&mut reply.handles, 0)?).read_bytes(length)
    }
}

/// 捕获域创建请求，副作用前完整检查外壳/句柄及参数；工厂提供者复用此入口。
/// # Errors
/// 未知版本、畸形输入、配置非法或资源不足。
pub fn configuration(
    request: &mut Reader<'_>,
    mut handles: Vec<Handle>,
) -> Result<(Configuration, RoutingSource, NonZeroU32), Status> {
    if request.u32()? != bgp::VERSION {
        return Err(native_abi::ERR_INVALID_ARGS);
    }
    let minimum = NonZeroU32::new(request.u32()?).ok_or(native_abi::ERR_INVALID_ARGS)?;
    let length = request.u64()?;
    request.finish()?;
    if handles.len() != 2 {
        return Err(native_abi::ERR_INVALID_ARGS);
    }
    let bytes = Vmo::from_handle(service_rpc::take_handle(&mut handles, 0)?).read_bytes(length)?;
    let configuration = Configuration::parse(&bytes).map_err(|_| native_abi::ERR_INVALID_ARGS)?;
    if configuration
        .neighbors
        .values()
        .any(|neighbor| neighbor.preference < minimum)
    {
        return Err(native_abi::ERR_ACCESS_DENIED);
    }
    let source = RoutingSource::new(Channel::from_handle(service_rpc::take_handle(
        &mut handles,
        1,
    )?));
    Ok((configuration, source, minimum))
}
