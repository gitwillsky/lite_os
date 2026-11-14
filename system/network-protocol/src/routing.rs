//! 接口配置 owner 到原生 RIB 的能力；复用安装/复合正文与事务传输，不开放执行 FIB 或独立下一跳写入。
use crate::{ExecutionPolicy, NetworkInstaller, PreparedConfiguration};
use alloc::vec::Vec;
use native_runtime::{
    Channel, Handle,
    native_abi::{self, Status},
};
use router_protocol::firewall::counters;
use router_protocol::{
    configuration::Name,
    connections::{Cursor, Identity, changes},
    interface::InterfaceConfiguration,
};
use service_rpc::{Reader, Writer};
mod configuration;
mod source;
mod tracking;
pub use configuration::Configuration;
pub use source::{
    Grant, PreparedSource, RoutingSource, SourcePreparation, SourceService, dispatch_source,
};
pub use tracking::{Tracking, TrackingService, dispatch_tracking};
const OPEN_SOURCE: u32 = 6;
const TRACK: u32 = 7;

/// 授权配置 owner 可委托独立来源能力；被委托者不能另开来源或改变授权。
pub trait RoutingConfigurationService: Send + Sync {
    /// 为实际转发执行线程应用全局专用 CPU。只有根配置能力可以调用。
    /// # Errors
    /// 作用域未授权、执行服务失效或隔离准入失败。
    fn set_forwarding_cpus(&self, mask: u64) -> Result<(), Status>;

    /// 打开具名域的配置与网络能力。现有域的路由来源与事务保持原身份。
    /// # Errors
    /// 工厂权限不足、NET 不可用或域资源预备失败。
    fn open_routing_domain(&self, name: Name) -> Result<(Channel, Channel), Status>;

    /// 经同一授权链捕获候选 VLAN 身份。
    /// # Errors
    /// 配置、设备或执行服务不可用。
    fn project_bridges(
        &self,
        ports: router_protocol::switching::Ports<'_>,
        plans: router_protocol::interface::InterfaceConfigurationBatch<'_>,
    ) -> Result<Vec<u8>, Status>;

    /// 读取获授权的本域规则计数页。
    /// # Errors
    /// 游标失效、域不可用或资源不足。
    fn firewall_counters(&self, cursor: counters::Cursor) -> Result<Vec<u8>, Status>;

    /// 经既有管理能力查询本域的一页连接。
    /// # Errors
    /// 游标失效或执行域不可用。
    fn connections(&self, cursor: Cursor) -> Result<Vec<u8>, Status>;

    /// 经授权管理能力清除本域内指定连接。
    /// # Errors
    /// 连接身份已失效或执行域不可用。
    fn clear_connection(&self, identity: Identity) -> Result<(), Status>;

    /// 读取获授权的连接变更页。
    /// # Errors
    /// 域不可用、资源不足或回复非法。
    fn connection_changes(&self, cursor: changes::Cursor) -> Result<Vec<u8>, Status>;

    /// 为已读到末尾的游标取得等待能力。
    /// # Errors
    /// 域不可用或资源不足。
    fn arm_connection_watch(
        &self,
        cursor: changes::Cursor,
    ) -> Result<Option<native_runtime::Event>, Status>;

    /// 派生本域双栈范围的独立只读跟踪能力，不创建额外路由来源。
    /// # Errors
    /// 查询非法、资源不足或服务不可用。
    fn track(
        &self,
        queries: Vec<router_protocol::tracking::Query>,
    ) -> Result<(Channel, native_runtime::Event), Status>;

    /// 根配置能力把授权网卡交给 NET 的指定域；`None` 表示默认域。
    /// # Errors
    /// 描述/能力非法、资源不足或服务失效。
    fn install_device(
        &self,
        id: u64,
        domain: Option<Name>,
        description: Vec<u8>,
        handles: Vec<Handle>,
    ) -> Result<(u32, Channel, native_runtime::Event), Status>;
    /// 预备接口、静态来源与可选转发策略；configuration=None 只改变执行策略，不接受执行 FIB。
    /// # Errors
    /// 格式/来源非法、资源不足、并发租约或服务失败。
    fn prepare_configuration(
        &self,
        configuration: Option<Configuration<'_>>,
        execution: ExecutionPolicy<'_>,
    ) -> Result<Channel, Status>;
    /// 授予独立来源与同域网络连接；关闭最后一个来源句柄会撤回该来源。
    /// # Errors
    /// 授权非法、事务冲突、资源不足或服务不可用。
    fn open_source(&self, grant: Grant) -> Result<(Channel, Channel), Status>;
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
    if ordinal == crate::installer::SET_FORWARDING_CPUS {
        crate::installer::dispatch_forwarding_cpus(request, handles, |mask| {
            service.set_forwarding_cpus(mask)
        })
    } else if ordinal == crate::installer::OPEN_ROUTING_DOMAIN {
        crate::installer::dispatch_open_routing_domain(request, handles, |name| {
            service.open_routing_domain(name)
        })
    } else if ordinal == crate::installer::bridge::PROJECT_BRIDGES {
        crate::installer::bridge::dispatch(request, handles, reply, |ports, plans| {
            service.project_bridges(ports, plans)
        })
    } else if ordinal == crate::installer::CONNECTIONS {
        crate::installer::dispatch_connections(request, handles, reply, |cursor| {
            service.connections(cursor)
        })
    } else if ordinal == crate::installer::CLEAR_CONNECTION {
        crate::installer::dispatch_clear_connection(request, handles, |identity| {
            service.clear_connection(identity)
        })
    } else if ordinal == crate::installer::CONNECTION_CHANGES {
        crate::installer::dispatch_connection_changes(request, handles, reply, |cursor| {
            service.connection_changes(cursor)
        })
    } else if ordinal == crate::installer::ARM_CONNECTION_WATCH {
        crate::installer::dispatch_arm_connection_watch(request, handles, |cursor| {
            service.arm_connection_watch(cursor)
        })
    } else if ordinal == crate::installer::FIREWALL_COUNTERS {
        crate::installer::dispatch_firewall_counters(request, handles, reply, |cursor| {
            service.firewall_counters(cursor)
        })
    } else if ordinal == TRACK {
        tracking::returned(|| service.track(tracking::queries(request, handles)?))
    } else if ordinal == OPEN_SOURCE {
        let grant = Grant::decode(request)?;
        request.finish()?;
        if !handles.is_empty() {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        let mut returned = Vec::new();
        returned
            .try_reserve_exact(2)
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        let (source, network) = service.open_source(grant)?;
        returned.push(source.into_handle());
        returned.push(network.into_handle());
        Ok(returned)
    } else if ordinal == crate::installer::INSTALL_DEVICE {
        crate::installer::dispatch_install_device(
            request,
            handles,
            reply,
            |id, domain, description, handles| {
                service.install_device(id, domain, description, handles)
            },
        )
    } else if ordinal == crate::installer::PREPARE_CONFIGURATION {
        let captured = super::configuration::capture_prepare(request, handles)?;
        let configuration = captured
            .body
            .as_deref()
            .map(Configuration::parse)
            .transpose()?;
        let mut returned = Vec::new();
        returned
            .try_reserve_exact(1)
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        returned.push(
            service
                .prepare_configuration(configuration, captured.policy()?)?
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
    /// 以根配置能力应用转发专用 CPU。`mask` 为零时恢复共享调度。
    /// # Errors
    /// 权限、执行实例或调度准入失败。
    pub fn set_forwarding_cpus(&self, mask: u64) -> Result<(), Status> {
        self.0.set_forwarding_cpus(mask)
    }
    /// 获取候选 VLAN 身份及链路事实。写入仍须提交同一拓扑和接口计划。
    /// # Errors
    /// 配置或拓扑已失效、内存不足或执行域不可用。
    pub fn project_bridges(
        &self,
        ports: router_protocol::switching::Ports<'_>,
        plans: &[InterfaceConfiguration<'_>],
    ) -> Result<Vec<u8>, Status> {
        let bytes = crate::installer::bridge::encode_plans(plans)?;
        let batch = if bytes.is_empty() {
            Default::default()
        } else {
            router_protocol::interface::InterfaceConfigurationBatch::parse(&bytes)
                .map_err(|_| native_abi::ERR_INVALID_ARGS)?
        };
        self.0.project_bridges(ports, batch)
    }

    /// 查询执行域中已安装规则的一页计数。
    /// # Errors
    /// 游标或回复无效、规则链换版或执行域不可用。
    pub fn firewall_counters(&self, cursor: counters::Cursor) -> Result<Vec<u8>, Status> {
        self.0.firewall_counters(cursor)
    }

    /// 读取获授权的默认域连接页正文。
    /// # Errors
    /// 游标失效、服务不可用或回复无效。
    pub fn connections(&self, cursor: Cursor) -> Result<Vec<u8>, Status> {
        self.0.connections(cursor)
    }

    /// 清除获授权的默认域连接。
    /// # Errors
    /// 身份已失效、服务不可用或回复无效。
    pub fn clear_connection(&self, identity: Identity) -> Result<(), Status> {
        self.0.clear_connection(identity)
    }

    /// 读取默认域的有界连接增量页。
    /// # Errors
    /// 域不可用、资源不足或回复非法。
    pub fn connection_changes(&self, cursor: changes::Cursor) -> Result<Vec<u8>, Status> {
        self.0.connection_changes(cursor)
    }

    /// 获取下一次变更的等待能力。落后或需重新同步时立即返回空。
    /// # Errors
    /// 域不可用、资源不足或回复非法。
    pub fn arm_connection_watch(
        &self,
        cursor: changes::Cursor,
    ) -> Result<Option<native_runtime::Event>, Status> {
        self.0.arm_connection_watch(cursor)
    }

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
    pub fn open_source(
        &self,
        grant: Grant,
    ) -> Result<(RoutingSource, crate::RemoteNamespace), Status> {
        grant.validate()?;
        let mut request = Writer::request(OPEN_SOURCE);
        request.try_reserve(10)?;
        grant.encode(&mut request);
        let reply = service_rpc::call(self.channel(), &request, Vec::new())?;
        reply.reader().finish()?;
        let [source, network] =
            <[Handle; 2]>::try_from(reply.handles).map_err(|_| native_abi::ERR_INVALID_ARGS)?;
        Ok((
            RoutingSource::new(Channel::from_handle(source)),
            crate::RemoteNamespace::from_channel(Channel::from_handle(network)),
        ))
    }
    /// 接管装配层授予的 RIB 连接，不从路由正文推导权限。
    pub fn new(channel: Channel) -> Self {
        Self(NetworkInstaller::new(channel))
    }
    /// 获取只管理指定域的配置与网络连接。
    /// # Errors
    /// 未获工厂权限、回复无效或 RIB/NET 不可用。
    pub fn open_routing_domain(
        &self,
        name: &Name,
    ) -> Result<(Self, crate::RemoteNamespace), Status> {
        let (controller, network) = self.0.open_routing_domain(name)?;
        Ok((Self(controller), network))
    }
    /// 提供者退出时置 PEER_CLOSED，用于重新连接并重放已持久决定的配置。
    pub fn channel(&self) -> &Channel {
        self.0.channel()
    }
    /// 经根 RIB 将授权网卡装入 `domain`，缺席时装入默认域，返回实际接口及寿命能力。
    /// # Errors
    /// 设备/能力非法、资源不足或服务失效。
    pub fn install_device(
        &self,
        id: u64,
        domain: Option<&Name>,
        description: &[u8],
        handles: Vec<Handle>,
    ) -> Result<(u32, Channel, native_runtime::Event), Status> {
        self.0.install_device(id, domain, description, handles)
    }
    /// 冻结接口配置与可选转发意图，返回同一激活/确认/回滚能力；有路由变化时由 RIB 选路。
    /// routes 按 IPv4/IPv6 排列，None 保持、显式空 Batch 撤回；只设置执行策略时不重编译 FIB。
    /// # Errors
    /// 无效输入、事务冲突、内存不足或服务不可用。
    pub fn prepare_configuration(
        &self,
        plans: &[InterfaceConfiguration],
        routes: [Option<&[u8]>; 2],
        policy: Option<&router_protocol::policy::Policy>,
        tables: Option<&[u8]>,
        execution: ExecutionPolicy<'_>,
    ) -> Result<PreparedConfiguration, Status> {
        let encoded = if plans.is_empty()
            && routes.iter().all(Option::is_none)
            && policy.is_none()
            && tables.is_none()
        {
            Vec::new()
        } else {
            Configuration::encode(plans, routes, policy, tables)?
        };
        let (request, handles) = super::configuration::prepare_request(encoded, execution)?;
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
