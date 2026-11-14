//! 网卡/FIB 执行协议（`service_directory::NETWORK_INSTALLER`，只路由给原生 RIB）。

use alloc::{string::String, vec::Vec};

use native_runtime::native_abi::{self, Status};
use native_runtime::{Channel, Event, Handle, Vmo};
use router_protocol::configuration::Name;
use router_protocol::connections::{Cursor, Identity, changes};
use router_protocol::firewall::counters;
use service_rpc::{Reader, Writer, call};
pub(crate) mod bridge;

pub(super) const INSTALL_DEVICE: u32 = 1;
const INSTALL_DEVICE_VERSION: u32 = 3;
pub(super) const PREPARE_CONFIGURATION: u32 = 2;
const PREPARE_NEXT_HOP: u32 = 4;
const WATCH_ROUTING_INPUTS: u32 = 5;
pub(super) const CONNECTIONS: u32 = 8;
pub(super) const CLEAR_CONNECTION: u32 = 9;
pub(super) const CONNECTION_CHANGES: u32 = 10;
pub(super) const ARM_CONNECTION_WATCH: u32 = 11;
pub(super) const FIREWALL_COUNTERS: u32 = 12;
pub(super) const OPEN_ROUTING_DOMAIN: u32 = 14;
const ROUTING_DOMAIN_VERSION: u32 = 1;
pub(super) const SET_FORWARDING_CPUS: u32 = 15;
const FORWARDING_CPUS_VERSION: u32 = 1;

/// 服务端：原生 RIB 独占的网卡/FIB 执行能力。
pub trait InstallerService: Send + Sync {
    /// 替换整个执行服务的转发专用核，空掩码回到共享调度。只允许根能力。
    /// # Errors
    /// 作用域无权、执行线程或 CPU 不可用、与现有调度分区冲突。
    fn set_forwarding_cpus(&self, mask: u64) -> Result<(), Status>;

    /// 取得同一具名域的执行与网络能力。只有初始安装入口可以打开其他域。
    /// # Errors
    /// 调用者没有工厂权限，或无法准备两个能力。
    fn open_routing_domain(&self, name: Name) -> Result<(Channel, Channel), Status>;

    /// 冻结候选 VLAN 接口身份，供同一批次的路由和规则解析。
    /// # Errors
    /// 配置非法、设备消失、预备冲突或内存不足。
    fn project_bridges(
        &self,
        ports: router_protocol::switching::Ports<'_>,
        plans: router_protocol::interface::InterfaceConfigurationBatch<'_>,
    ) -> Result<Vec<u8>, Status>;

    /// 读取本域当前规则链的一页命中计数。
    /// # Errors
    /// 网络域不可用、游标失效或资源不足。
    fn firewall_counters(&self, cursor: counters::Cursor) -> Result<Vec<u8>, Status>;

    /// 读取本域的一页连接正文。
    /// # Errors
    /// 过期游标、域不可用、资源不足或回复非法。
    fn connections(&self, cursor: Cursor) -> Result<Vec<u8>, Status>;

    /// 清除由实例和 ID 共同指定的活动连接。
    /// # Errors
    /// 身份已失效或网络域不可用。
    fn clear_connection(&self, identity: Identity) -> Result<(), Status>;

    /// 读取本域的连接增量正文。
    /// # Errors
    /// 域不可用、资源不足或回复非法。
    fn connection_changes(&self, cursor: changes::Cursor) -> Result<Vec<u8>, Status>;

    /// 游标已读到末尾时返回下一次变化的等待能力，否则立即返回空。
    /// # Errors
    /// 域不可用或资源不足。
    fn arm_connection_watch(&self, cursor: changes::Cursor) -> Result<Option<Event>, Status>;

    /// 订阅执行域路由输入的合并通知。消费者清除并重新武装所需信号，然后读取对应快照。
    /// # Errors
    /// 能力不授权、资源不足或执行域失效。
    fn watch_routing_inputs(&self) -> Result<native_runtime::Event, Status>;
    /// 预备更新一个具名下一跳；所有引用保留身份，不逐前缀修改。
    /// # Errors
    /// 对象不存在、字段/设备/网关无效、引用范围不允许、事务冲突或资源不足。
    fn prepare_next_hop(
        &self,
        definition: super::routes::NextHopConfiguration<'_, core::net::Ipv4Addr>,
    ) -> Result<Channel, Status>;

    /// 预备接口/FIB 与可选 IPv4 转发策略，返回同一确认/回滚能力；预备不改变活动状态。
    /// `batch=None` 只允许显式转发策略；策略 None 保持当前运行值，不默认为开启。
    /// # Errors
    /// 配置冲突、资源不足、设备/网关无效或协议不可用；失败不保留配置租约。
    fn prepare_configuration(
        &self,
        batch: Option<super::ConfigurationBatch<'_>>,
        forwarding: Option<crate::Forwarding>,
    ) -> Result<Channel, Status>;

    /// 以根安装能力将授权目录中的设备放入选定域。`None` 表示默认域，`handles` 只携带目录能力。
    ///
    /// # Returns
    ///
    /// 接口序号，以及接口的 lifetime channel：接口移除（网卡会话关闭）时服务端关闭对端，
    /// 独立 Event 以 USER_0 通知此网卡链路变化。使用者在寿命通道观察到 `PEER_CLOSED`（同 Fuchsia `interfaces.admin.Control` 的移除终止）。
    ///
    /// # Errors
    ///
    /// 权限不足、描述或句柄不完整、目标域不存在、旧域有在途事务、连接失败或资源不足。
    fn install_device(
        &self,
        device_id: u64,
        domain: Option<Name>,
        description: Vec<u8>,
        handles: Vec<Handle>,
    ) -> Result<(u32, Channel, native_runtime::Event), Status>;
}

/// 分派安装 channel 的一条请求；签名与 `service_rpc::Handler` 一致。
///
/// # Errors
///
/// 未知请求、格式错误或服务的失败。
pub fn dispatch_installer(
    service: &dyn InstallerService,
    ordinal: u32,
    request: &mut Reader<'_>,
    handles: Vec<Handle>,
    reply: &mut Writer,
) -> Result<Vec<Handle>, Status> {
    match ordinal {
        SET_FORWARDING_CPUS => {
            dispatch_forwarding_cpus(request, handles, |mask| service.set_forwarding_cpus(mask))
        }
        OPEN_ROUTING_DOMAIN => {
            dispatch_open_routing_domain(request, handles, |name| service.open_routing_domain(name))
        }
        bridge::PROJECT_BRIDGES => bridge::dispatch(request, handles, reply, |ports, plans| {
            service.project_bridges(ports, plans)
        }),
        CONNECTIONS => dispatch_connections(request, handles, reply, |cursor| {
            service.connections(cursor)
        }),
        CLEAR_CONNECTION => dispatch_clear_connection(request, handles, |identity| {
            service.clear_connection(identity)
        }),
        CONNECTION_CHANGES => dispatch_connection_changes(request, handles, reply, |cursor| {
            service.connection_changes(cursor)
        }),
        ARM_CONNECTION_WATCH => dispatch_arm_connection_watch(request, handles, |cursor| {
            service.arm_connection_watch(cursor)
        }),
        FIREWALL_COUNTERS => dispatch_firewall_counters(request, handles, reply, |cursor| {
            service.firewall_counters(cursor)
        }),
        WATCH_ROUTING_INPUTS => {
            request.finish()?;
            if !handles.is_empty() {
                return Err(native_abi::ERR_INVALID_ARGS);
            }
            let mut returned = Vec::new();
            returned
                .try_reserve_exact(1)
                .map_err(|_| native_abi::ERR_NO_MEMORY)?;
            returned.push(service.watch_routing_inputs()?.into_handle());
            Ok(returned)
        }
        PREPARE_NEXT_HOP => {
            let length = request.u64()?;
            request.finish()?;
            if handles.len() != 1 {
                return Err(native_abi::ERR_INVALID_ARGS);
            }
            let mut handles = handles;
            let content =
                Vmo::from_handle(service_rpc::take_handle(&mut handles, 0)?).read_bytes(length)?;
            let definition = super::routes::NextHopConfiguration::parse(&content)?;
            let mut returned = Vec::new();
            returned
                .try_reserve_exact(1)
                .map_err(|_| native_abi::ERR_NO_MEMORY)?;
            returned.push(service.prepare_next_hop(definition)?.into_handle());
            Ok(returned)
        }
        PREPARE_CONFIGURATION => {
            let captured = super::configuration::capture_prepare(request, handles)?;
            let policy = captured.policy()?;
            let batch = captured
                .body
                .as_deref()
                .map(super::ConfigurationBatch::parse)
                .transpose()
                .map_err(|_| native_abi::ERR_INVALID_ARGS)?;
            let mut returned = Vec::new();
            returned
                .try_reserve_exact(1)
                .map_err(|_| native_abi::ERR_NO_MEMORY)?;
            let batch = match policy.prerouting {
                Some(chain) => Some(batch.unwrap_or_default().with_prerouting(chain)),
                None => batch,
            };
            let batch = match policy.bridges {
                Some(bridges) => Some(batch.unwrap_or_default().with_bridges(bridges)),
                None => batch,
            };
            let batch = match policy.multicast {
                Some(multicast) => Some(batch.unwrap_or_default().with_multicast(multicast)),
                None => batch,
            };
            returned.push(
                service
                    .prepare_configuration(batch, policy.forwarding)?
                    .into_handle(),
            );
            Ok(returned)
        }
        INSTALL_DEVICE => dispatch_install_device(
            request,
            handles,
            reply,
            |id, domain, description, handles| {
                service.install_device(id, domain, description, handles)
            },
        ),
        _ => Err(native_abi::ERR_NOT_SUPPORTED),
    }
}

pub(super) fn dispatch_forwarding_cpus(
    request: &mut Reader<'_>,
    handles: Vec<Handle>,
    apply: impl FnOnce(u64) -> Result<(), Status>,
) -> Result<Vec<Handle>, Status> {
    if !handles.is_empty() || request.u32()? != FORWARDING_CPUS_VERSION {
        return Err(native_abi::ERR_INVALID_ARGS);
    }
    let mask = request.u64()?;
    request.finish()?;
    apply(mask)?;
    Ok(Vec::new())
}

pub(super) fn dispatch_open_routing_domain(
    request: &mut Reader<'_>,
    handles: Vec<Handle>,
    open: impl FnOnce(Name) -> Result<(Channel, Channel), Status>,
) -> Result<Vec<Handle>, Status> {
    if !handles.is_empty() || request.u32()? != ROUTING_DOMAIN_VERSION {
        return Err(native_abi::ERR_INVALID_ARGS);
    }
    let name = routing_domain_name(request.bytes()?)?;
    request.finish()?;
    let mut returned = Vec::new();
    returned
        .try_reserve_exact(2)
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    let (installer, network) = open(name)?;
    returned.push(installer.into_handle());
    returned.push(network.into_handle());
    Ok(returned)
}

fn routing_domain_name(bytes: &[u8]) -> Result<Name, Status> {
    let name = core::str::from_utf8(bytes).map_err(|_| native_abi::ERR_INVALID_ARGS)?;
    let mut owned = String::new();
    owned
        .try_reserve_exact(name.len())
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    owned.push_str(name);
    Name::try_from(owned).map_err(|_| native_abi::ERR_INVALID_ARGS)
}

pub(super) fn dispatch_firewall_counters(
    request: &mut Reader<'_>,
    handles: Vec<Handle>,
    reply: &mut Writer,
    query: impl FnOnce(counters::Cursor) -> Result<Vec<u8>, Status>,
) -> Result<Vec<Handle>, Status> {
    let cursor =
        counters::Cursor::parse(request.bytes()?).map_err(|_| native_abi::ERR_INVALID_ARGS)?;
    request.finish()?;
    if !handles.is_empty() {
        return Err(native_abi::ERR_INVALID_ARGS);
    }
    write_bounded_reply(reply, &query(cursor)?, counters::MAX_BYTES)
}

pub(super) fn dispatch_connections(
    request: &mut Reader<'_>,
    handles: Vec<Handle>,
    reply: &mut Writer,
    query: impl FnOnce(Cursor) -> Result<Vec<u8>, Status>,
) -> Result<Vec<Handle>, Status> {
    use router_protocol::connections::MAX_PAGE_BYTES;
    let cursor = Cursor::parse(request.bytes()?).map_err(|_| native_abi::ERR_INVALID_ARGS)?;
    request.finish()?;
    if !handles.is_empty() {
        return Err(native_abi::ERR_INVALID_ARGS);
    }
    write_bounded_reply(reply, &query(cursor)?, MAX_PAGE_BYTES as usize)
}

fn write_bounded_reply(
    reply: &mut Writer,
    bytes: &[u8],
    maximum: usize,
) -> Result<Vec<Handle>, Status> {
    if bytes.len() > maximum {
        return Err(native_abi::ERR_OUT_OF_RANGE);
    }
    reply.try_reserve(4 + bytes.len())?;
    reply.bytes(bytes);
    Ok(Vec::new())
}

fn change_cursor(request: &mut Reader<'_>, handles: &[Handle]) -> Result<changes::Cursor, Status> {
    let cursor =
        changes::Cursor::parse(request.bytes()?).map_err(|_| native_abi::ERR_INVALID_ARGS)?;
    request.finish()?;
    if !handles.is_empty() {
        return Err(native_abi::ERR_INVALID_ARGS);
    }
    Ok(cursor)
}

pub(super) fn dispatch_connection_changes(
    request: &mut Reader<'_>,
    handles: Vec<Handle>,
    reply: &mut Writer,
    query: impl FnOnce(changes::Cursor) -> Result<Vec<u8>, Status>,
) -> Result<Vec<Handle>, Status> {
    let cursor = change_cursor(request, &handles)?;
    write_bounded_reply(reply, &query(cursor)?, changes::MAX_BYTES)
}

pub(super) fn dispatch_arm_connection_watch(
    request: &mut Reader<'_>,
    handles: Vec<Handle>,
    arm: impl FnOnce(changes::Cursor) -> Result<Option<Event>, Status>,
) -> Result<Vec<Handle>, Status> {
    let cursor = change_cursor(request, &handles)?;
    let mut handles = Vec::new();
    handles
        .try_reserve_exact(1)
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    if let Some(event) = arm(cursor)? {
        handles.push(event.into_handle());
    }
    Ok(handles)
}

pub(super) fn dispatch_clear_connection(
    request: &mut Reader<'_>,
    handles: Vec<Handle>,
    clear: impl FnOnce(Identity) -> Result<(), Status>,
) -> Result<Vec<Handle>, Status> {
    let identity = Identity::parse(request.bytes()?).map_err(|_| native_abi::ERR_INVALID_ARGS)?;
    request.finish()?;
    if !handles.is_empty() {
        return Err(native_abi::ERR_INVALID_ARGS);
    }
    clear(identity)?;
    Ok(Vec::new())
}

pub(super) fn dispatch_install_device(
    request: &mut Reader<'_>,
    handles: Vec<Handle>,
    reply: &mut Writer,
    install: impl FnOnce(
        u64,
        Option<Name>,
        Vec<u8>,
        Vec<Handle>,
    ) -> Result<(u32, Channel, native_runtime::Event), Status>,
) -> Result<Vec<Handle>, Status> {
    if request.u32()? != INSTALL_DEVICE_VERSION {
        return Err(native_abi::ERR_INVALID_ARGS);
    }
    let device_id = request.u64()?;
    let name = request.bytes()?;
    let domain = (!name.is_empty())
        .then(|| routing_domain_name(name))
        .transpose()?;
    let description = request.bytes()?;
    request.finish()?;
    // 安装之后不能再因回复扩容失败而丢失刚创建的接口身份/寿命能力。
    reply.try_reserve(4)?;
    let mut returned = Vec::new();
    returned
        .try_reserve_exact(2)
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    let mut copy = Vec::new();
    copy.try_reserve_exact(description.len())
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    copy.extend_from_slice(description);
    let (index, lifetime, link_changed) = install(device_id, domain, copy, handles)?;
    reply.u32(index);
    returned.push(lifetime.into_handle());
    returned.push(link_changed.into_handle());
    Ok(returned)
}

/// 使用者端：网卡安装。
pub struct NetworkInstaller {
    channel: Channel,
}

impl NetworkInstaller {
    /// 用根安装能力设置实际转发执行线程的 CPU 归属。`mask` 为零时恢复共享调度。
    /// # Errors
    /// 作用域未授权、隔离准入失败或执行实例失效。
    pub fn set_forwarding_cpus(&self, mask: u64) -> Result<(), Status> {
        let mut request = Writer::request(SET_FORWARDING_CPUS);
        request.try_reserve(12)?;
        request.u32(FORWARDING_CPUS_VERSION).u64(mask);
        let reply = call(&self.channel, &request, Vec::new())?;
        if !reply.handles.is_empty() {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        reply.reader().finish()
    }
    /// 打开具名域，返回固定作用域的安装与网络连接。
    /// # Errors
    /// 调用方无权打开其他域，或服务无法创建、捕获域能力。
    pub fn open_routing_domain(
        &self,
        name: &Name,
    ) -> Result<(Self, super::RemoteNamespace), Status> {
        let mut request = Writer::request(OPEN_ROUTING_DOMAIN);
        request.try_reserve(8 + name.as_str().len())?;
        request
            .u32(ROUTING_DOMAIN_VERSION)
            .bytes(name.as_str().as_bytes());
        let reply = call(&self.channel, &request, Vec::new())?;
        reply.reader().finish()?;
        let [installer, network] =
            <[Handle; 2]>::try_from(reply.handles).map_err(|_| native_abi::ERR_INVALID_ARGS)?;
        Ok((
            Self::new(Channel::from_handle(installer)),
            super::RemoteNamespace::from_channel(Channel::from_handle(network)),
        ))
    }

    /// 请求 NET 的候选桥投影，既不创建接口也不保留写租约。
    /// # Errors
    /// 配置、设备、协议或资源失败。
    pub fn project_bridges(
        &self,
        ports: router_protocol::switching::Ports<'_>,
        plans: router_protocol::interface::InterfaceConfigurationBatch<'_>,
    ) -> Result<Vec<u8>, Status> {
        bridge::request(&self.channel, ports, plans)
    }

    /// 读取一页规则计数正文。
    /// # Errors
    /// 规则链换版、游标或回复非法，或域不可用。
    pub fn firewall_counters(&self, cursor: counters::Cursor) -> Result<Vec<u8>, Status> {
        self.read_bounded_bytes(FIREWALL_COUNTERS, &cursor.encode(), counters::MAX_BYTES)
    }

    /// 从获授权的网络域读取一页连接，返回唯一分页 codec 的正文。
    /// # Errors
    /// 游标失效、畸形回复、资源不足或服务不可用。
    pub fn connections(&self, cursor: Cursor) -> Result<Vec<u8>, Status> {
        self.read_bounded_bytes(
            CONNECTIONS,
            &cursor.encode(),
            router_protocol::connections::MAX_PAGE_BYTES as usize,
        )
    }

    /// 读取增量页，返回唯一变更 codec 的正文。
    /// # Errors
    /// 回复畸形、资源不足或服务不可用。
    pub fn connection_changes(&self, cursor: changes::Cursor) -> Result<Vec<u8>, Status> {
        self.read_bounded_bytes(CONNECTION_CHANGES, &cursor.encode(), changes::MAX_BYTES)
    }

    fn read_bounded_bytes(
        &self,
        ordinal: u32,
        selector: &[u8],
        maximum: usize,
    ) -> Result<Vec<u8>, Status> {
        let response = self.selector_request(ordinal, selector)?;
        if !response.handles.is_empty() {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        let mut reader = response.reader();
        let bytes = reader.bytes()?;
        reader.finish()?;
        if bytes.len() > maximum {
            return Err(native_abi::ERR_OUT_OF_RANGE);
        }
        let mut page = Vec::new();
        page.try_reserve_exact(bytes.len())
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        page.extend_from_slice(bytes);
        Ok(page)
    }

    /// 清除目标转发域中的一条连接并确认完成。
    /// # Errors
    /// 身份已失效、回复畸形或服务不可用。
    pub fn clear_connection(&self, identity: Identity) -> Result<(), Status> {
        let response = self.selector_request(CLEAR_CONNECTION, &identity.encode())?;
        if !response.handles.is_empty() {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        response.reader().finish()
    }

    /// 取得只读的下一次连接变化等待能力，或立即重查的空回复。
    /// # Errors
    /// 回复畸形、资源不足或服务不可用。
    pub fn arm_connection_watch(&self, cursor: changes::Cursor) -> Result<Option<Event>, Status> {
        let response = self.selector_request(ARM_CONNECTION_WATCH, &cursor.encode())?;
        response.reader().finish()?;
        let mut handles = response.handles.into_iter();
        let event = handles.next().map(Event::from_handle);
        if handles.next().is_some() {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        Ok(event)
    }

    fn selector_request(
        &self,
        ordinal: u32,
        selector: &[u8],
    ) -> Result<service_rpc::Reply, Status> {
        let mut request = Writer::request(ordinal);
        request.try_reserve(4 + selector.len())?;
        request.bytes(selector);
        call(&self.channel, &request, Vec::new())
    }

    /// 接口、地址或链路事实需要重新捕获。
    pub const TOPOLOGY_CHANGED: u32 = native_abi::signals::USER_0;
    /// 网关的已知失败集合变化，需要重新捕获双栈邻居事实。
    pub const GATEWAYS_CHANGED: u32 = native_abi::signals::USER_1;

    /// 取得路由输入的合并通知。两个信号可独立武装，信号不包含状态或历史。
    /// # Errors
    /// 资源不足、畸形回复或执行服务不可用。
    pub fn watch_routing_inputs(&self) -> Result<native_runtime::Event, Status> {
        let mut reply = call(
            &self.channel,
            &Writer::request(WATCH_ROUTING_INPUTS),
            Vec::new(),
        )?;
        reply.reader().finish()?;
        if reply.handles.len() != 1 {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        Ok(native_runtime::Event::from_handle(
            service_rpc::take_handle(&mut reply.handles, 0)?,
        ))
    }
    pub fn new(channel: Channel) -> Self {
        Self { channel }
    }

    /// 连接的 channel：netstack 终止时置位 `PEER_CLOSED`，使用者据此重新安装。
    pub fn channel(&self) -> &Channel {
        &self.channel
    }

    /// 预备具名下一跳更新，沿同一确认/撤销能力执行；不重新下发引用它的前缀。
    /// # Errors
    /// 对象不存在、路径/引用范围无效、事务冲突、资源不足或服务不可用。
    pub fn prepare_next_hop(
        &self,
        definition: super::routes::NextHopConfiguration<'_, core::net::Ipv4Addr>,
    ) -> Result<super::PreparedConfiguration, Status> {
        let encoded = definition.encode()?;
        let content = Vmo::from_bytes(encoded.as_bytes())?;
        let mut request = Writer::request(PREPARE_NEXT_HOP);
        request.try_reserve(8)?;
        request.u64(encoded.as_bytes().len() as u64);
        // VMO 已有独立只读正文；远端准备大候选期间不再保留客户端整份编码副本。
        drop(encoded);
        let mut handles = Vec::new();
        handles
            .try_reserve_exact(1)
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        handles.push(content.into_handle());
        self.call_prepared(&request, handles)
    }

    fn call_prepared(
        &self,
        request: &Writer,
        handles: Vec<Handle>,
    ) -> Result<super::PreparedConfiguration, Status> {
        let mut reply = call(&self.channel, request, handles)?;
        if reply.handles.len() != 1 {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        reply.reader().finish()?;
        Ok(super::PreparedConfiguration::new(Channel::from_handle(
            service_rpc::take_handle(&mut reply.handles, 0)?,
        )))
    }

    /// 预备已编译接口/FIB 与可选转发策略；关闭或 abort 撤销未确认变更。
    /// # Parameters
    /// `encoded` 由 ConfigurationBatch 的唯一 codec 构造并移交；只有接口/FIB 实际变更才为该正文创建 VMO。
    /// 策略字段缺席时保持运行值；本层只拥有传输，编码存储在 RPC 前释放，服务端仍完整验证。
    /// # Errors
    /// 正文/配置非法、事务冲突、执行服务拒绝、资源不足或服务不可用。
    pub fn prepare_configuration(
        &self,
        encoded: Vec<u8>,
        policy: crate::ExecutionPolicy<'_>,
    ) -> Result<super::PreparedConfiguration, Status> {
        let (request, handles) = super::configuration::prepare_request(encoded, policy)?;
        self.call_prepared(&request, handles)
    }

    /// 把网卡装入 `domain`，缺席表示默认域。返回接口序号、寿命通道与链路事件。
    ///
    /// # Errors
    ///
    /// 服务拒绝该设备或已终止。
    pub fn install_device(
        &self,
        device_id: u64,
        domain: Option<&Name>,
        description: &[u8],
        handles: Vec<Handle>,
    ) -> Result<(u32, Channel, native_runtime::Event), Status> {
        let name = domain.map_or(&[][..], |name| name.as_str().as_bytes());
        let mut writer = Writer::request(INSTALL_DEVICE);
        writer.try_reserve(
            20usize
                .checked_add(description.len())
                .and_then(|size| size.checked_add(name.len()))
                .ok_or(native_abi::ERR_OUT_OF_RANGE)?,
        )?;
        writer
            .u32(INSTALL_DEVICE_VERSION)
            .u64(device_id)
            .bytes(name)
            .bytes(description);
        let mut reply = call(&self.channel, &writer, handles)?;
        if reply.handles.len() != 2 {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        let (mut reader, handles) = reply.split();
        let index = reader.u32()?;
        reader.finish()?;
        let link_changed =
            native_runtime::Event::from_handle(service_rpc::take_handle(handles, 1)?);
        let lifetime = Channel::from_handle(service_rpc::take_handle(handles, 0)?);
        Ok((index, lifetime, link_changed))
    }
}
