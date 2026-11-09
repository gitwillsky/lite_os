//! 接口配置 owner 到原生 RIB 的能力；复用安装/复合正文与事务传输，不开放执行 FIB 或独立下一跳写入。
use crate::{NetworkInstaller, PreparedConfiguration};
use alloc::vec::Vec;
use native_runtime::{
    Channel, Handle,
    native_abi::{self, Status},
};
use router_protocol::interface::Ipv4InterfaceConfiguration;
use service_rpc::{Reader, Writer};
mod configuration;
mod source;
mod tracking;
pub use configuration::Configuration;
pub use source::{Grant, RoutingSource, SourceService, dispatch_source};
pub use tracking::{Tracking, TrackingService, dispatch_tracking};
const OPEN_SOURCE: u32 = 6;
const TRACK: u32 = 7;

/// 授权配置 owner 可委托独立来源能力；被委托者不能另开来源或改变授权。
pub trait RoutingConfigurationService: Send + Sync {
    /// 派生默认 IPv4 范围的独立只读跟踪能力，不创建额外路由来源。
    /// # Errors
    /// 查询非法、资源不足或服务不可用。
    fn track(
        &self,
        queries: Vec<router_protocol::tracking::Query>,
    ) -> Result<(Channel, native_runtime::Event), Status>;

    /// 把授权网卡交给 NET；只转交设备能力，不直接取得执行写权限。
    /// # Errors
    /// 描述/能力非法、资源不足或服务失效。
    fn install_device(
        &self,
        id: u64,
        description: Vec<u8>,
        handles: Vec<Handle>,
    ) -> Result<(u32, Channel), Status>;
    /// 预备接口意图和可选静态来源的复合变更；不接受执行 FIB。
    /// # Errors
    /// 格式/来源非法、资源不足、并发租约或服务失败。
    fn prepare_configuration(&self, configuration: Configuration<'_>) -> Result<Channel, Status>;
    /// 创建一份独立来源寿命能力；关闭最后一个客户端句柄会撤回该来源。
    /// # Errors
    /// 授权非法、事务冲突、资源不足或服务不可用。
    fn open_source(&self, grant: Grant) -> Result<Channel, Status>;
}

/// 分派获授权配置来源的请求；拒绝直接 FIB 和下一跳更新，正文仍由 RIB 完整校验。
/// # Errors
/// 请求/句柄畸形、操作不在此能力内或服务失败。
pub fn dispatch_routing_configuration(
    service: &dyn RoutingConfigurationService,
    ordinal: u32,
    request: &mut Reader<'_>,
    handles: Vec<Handle>,
    reply: &mut Writer,
) -> Result<Vec<Handle>, Status> {
    if ordinal == TRACK {
        tracking::returned(|| service.track(tracking::queries(request, handles)?))
    } else if ordinal == OPEN_SOURCE {
        let grant = Grant::decode(request)?;
        request.finish()?;
        if !handles.is_empty() {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        let mut returned = Vec::new();
        returned
            .try_reserve_exact(1)
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        returned.push(service.open_source(grant)?.into_handle());
        Ok(returned)
    } else if ordinal == crate::installer::INSTALL_DEVICE {
        crate::installer::dispatch_install_device(
            request,
            handles,
            reply,
            |id, description, handles| service.install_device(id, description, handles),
        )
    } else if ordinal == crate::installer::PREPARE_CONFIGURATION {
        let length = request.u64()?;
        request.finish()?;
        if handles.len() != 1 {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        let mut returned = Vec::new();
        returned
            .try_reserve_exact(1)
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        let mut handles = handles;
        let bytes = native_runtime::Vmo::from_handle(service_rpc::take_handle(&mut handles, 0)?)
            .read_bytes(length)?;
        returned.push(
            service
                .prepare_configuration(Configuration::parse(&bytes)?)?
                .into_handle(),
        );
        Ok(returned)
    } else {
        Err(native_abi::ERR_NOT_SUPPORTED)
    }
}

/// 仅 router-config 获得的配置来源；没有向 netstack 直接提交 FIB 的方法。
pub struct RoutingConfigurator(NetworkInstaller);
impl RoutingConfigurator {
    /// 直接派生只读跟踪会话；不需要为观察创建空来源。
    /// # Errors
    /// 查询非法、资源不足、回复畸形或服务不可用。
    pub fn track(&self, queries: &[router_protocol::tracking::Query]) -> Result<Tracking, Status> {
        let (request, handles) = tracking::request(TRACK, queries)?;
        Tracking::from_reply(service_rpc::call(self.channel(), &request, handles)?)
    }

    /// 按可信授权创建独立来源，可移交协议进程；正文不能改变协议标记/最低偏好或来源身份。
    /// # Errors
    /// 授权非法、事务冲突、资源不足或服务不可用。
    pub fn open_source(&self, grant: Grant) -> Result<RoutingSource, Status> {
        grant.validate()?;
        let mut request = Writer::request(OPEN_SOURCE);
        request.try_reserve(9)?;
        grant.encode(&mut request);
        let mut reply = service_rpc::call(self.channel(), &request, Vec::new())?;
        reply.reader().finish()?;
        if reply.handles.len() != 1 {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        Ok(RoutingSource::new(Channel::from_handle(
            service_rpc::take_handle(&mut reply.handles, 0)?,
        )))
    }
    /// 接管装配层授予的 RIB 连接，不从路由正文推导权限。
    pub fn new(channel: Channel) -> Self {
        Self(NetworkInstaller::new(channel))
    }
    /// 提供者退出时置 PEER_CLOSED，用于重新连接并重放已持久决定的配置。
    pub fn channel(&self) -> &Channel {
        self.0.channel()
    }
    /// 经 RIB 把授权网卡交给执行服务；返回实际接口身份及寿命能力。
    /// # Errors
    /// 设备/能力非法、资源不足或服务失效。
    pub fn install_device(
        &self,
        id: u64,
        description: &[u8],
        handles: Vec<Handle>,
    ) -> Result<(u32, Channel), Status> {
        self.0.install_device(id, description, handles)
    }
    /// 冻结接口配置意图，返回同一激活/确认/回滚能力；选路与执行正文由 RIB 拥有。
    /// # Errors
    /// 无效输入、事务冲突、内存不足或服务不可用。
    pub fn prepare_configuration(
        &self,
        plans: &[Ipv4InterfaceConfiguration],
        routes: Option<&[u8]>,
        policy: Option<&router_protocol::policy::Policy>,
    ) -> Result<PreparedConfiguration, Status> {
        let encoded = Configuration::encode(plans, routes, policy)?;
        let content = native_runtime::Vmo::from_bytes(&encoded)?;
        let mut request = Writer::request(crate::installer::PREPARE_CONFIGURATION);
        request.try_reserve(8)?;
        request.u64(encoded.len() as u64);
        drop(encoded);
        let mut handles = Vec::new();
        handles
            .try_reserve_exact(1)
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        handles.push(content.into_handle());
        let mut reply = service_rpc::call(self.channel(), &request, handles)?;
        reply.reader().finish()?;
        if reply.handles.len() != 1 {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        Ok(PreparedConfiguration::new(Channel::from_handle(
            service_rpc::take_handle(&mut reply.handles, 0)?,
        )))
    }
}
