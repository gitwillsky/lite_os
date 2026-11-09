//! 网卡/FIB 执行协议（`service_directory::NETWORK_INSTALLER`，只路由给原生 RIB）。

use alloc::vec::Vec;

use native_runtime::native_abi::{self, Status};
use native_runtime::{Channel, Handle, Vmo};
use service_rpc::{Reader, Writer, call};

pub(super) const INSTALL_DEVICE: u32 = 1;
pub(super) const PREPARE_CONFIGURATION: u32 = 2;
const PREPARE_NEXT_HOP: u32 = 4;
const WATCH_INTERFACES: u32 = 5;

/// 服务端：原生 RIB 独占的网卡/FIB 执行能力。
pub trait InstallerService: Send + Sync {
    /// 订阅实际接口变化的合并通知；消费者 clear/arm 后重新读快照，不从事件重建接口状态。
    /// # Errors
    /// 能力不授权、资源不足或执行域失效。
    fn watch_interfaces(&self) -> Result<native_runtime::Event, Status>;
    /// 预备更新一个具名下一跳；所有引用保留身份，不逐前缀修改。
    /// # Errors
    /// 对象不存在、字段/设备/网关无效、引用范围不允许、事务冲突或资源不足。
    fn prepare_next_hop(
        &self,
        definition: super::routes::NextHopConfiguration<'_>,
    ) -> Result<Channel, Status>;

    /// 预备接口与可选完整 FIB，并返回独立的确认/回滚能力；预备不改变活动状态。
    ///
    /// # Errors
    /// 配置冲突、资源不足、设备/网关无效或协议不可用；失败不保留配置租约。
    fn prepare_configuration(
        &self,
        batch: super::ConfigurationBatch<'_>,
    ) -> Result<Channel, Status>;

    /// 把授权目录中的 device_id 安装为新接口；netstack 按 worker 配置打开会话，handles 只携带目录能力。
    ///
    /// # Returns
    ///
    /// 接口序号，以及接口的 lifetime channel：接口移除（网卡会话关闭）时服务端关闭对端，
    /// 使用者在其上观察到 `PEER_CLOSED`（同 Fuchsia `interfaces.admin.Control` 的移除终止）。
    ///
    /// # Errors
    ///
    /// 描述或句柄不完整、连接失败或资源不足。
    fn install_device(
        &self,
        device_id: u64,
        description: Vec<u8>,
        handles: Vec<Handle>,
    ) -> Result<(u32, Channel), Status>;
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
        WATCH_INTERFACES => {
            request.finish()?;
            if !handles.is_empty() {
                return Err(native_abi::ERR_INVALID_ARGS);
            }
            let mut returned = Vec::new();
            returned
                .try_reserve_exact(1)
                .map_err(|_| native_abi::ERR_NO_MEMORY)?;
            returned.push(service.watch_interfaces()?.into_handle());
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
            let length = request.u64()?;
            request.finish()?;
            if handles.len() != 1 {
                return Err(native_abi::ERR_INVALID_ARGS);
            }
            let mut handles = handles;
            let content =
                Vmo::from_handle(service_rpc::take_handle(&mut handles, 0)?).read_bytes(length)?;
            let batch = super::ConfigurationBatch::parse(&content)
                .map_err(|_| native_abi::ERR_INVALID_ARGS)?;
            let mut returned = Vec::new();
            returned
                .try_reserve_exact(1)
                .map_err(|_| native_abi::ERR_NO_MEMORY)?;
            returned.push(service.prepare_configuration(batch)?.into_handle());
            Ok(returned)
        }
        INSTALL_DEVICE => {
            dispatch_install_device(request, handles, reply, |id, description, handles| {
                service.install_device(id, description, handles)
            })
        }
        _ => Err(native_abi::ERR_NOT_SUPPORTED),
    }
}

pub(super) fn dispatch_install_device(
    request: &mut Reader<'_>,
    handles: Vec<Handle>,
    reply: &mut Writer,
    install: impl FnOnce(u64, Vec<u8>, Vec<Handle>) -> Result<(u32, Channel), Status>,
) -> Result<Vec<Handle>, Status> {
    let device_id = request.u64()?;
    let description = request.bytes()?;
    request.finish()?;
    // 安装之后不能再因回复扩容失败而丢失刚创建的接口身份/寿命能力。
    reply.try_reserve(4)?;
    let mut returned = Vec::new();
    returned
        .try_reserve_exact(1)
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    let mut copy = Vec::new();
    copy.try_reserve_exact(description.len())
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    copy.extend_from_slice(description);
    let (index, lifetime) = install(device_id, copy, handles)?;
    reply.u32(index);
    returned.push(lifetime.into_handle());
    Ok(returned)
}

/// 使用者端：网卡安装。
pub struct NetworkInstaller {
    channel: Channel,
}

impl NetworkInstaller {
    /// 取得实际接口/地址/MTU/管理状态/载波变化的合并通知；清除并武装后读取完整快照，允许合并中间变化。
    /// # Errors
    /// 资源不足、畸形回复或执行服务不可用。
    pub fn watch_interfaces(&self) -> Result<native_runtime::Event, Status> {
        let mut reply = call(
            &self.channel,
            &Writer::request(WATCH_INTERFACES),
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
        definition: super::routes::NextHopConfiguration<'_>,
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

    /// 通过单个只读 VMO 预备已经编译的接口/FIB 正文；关闭或 abort 撤销未确认变更。
    /// # Parameters
    /// `encoded` 由 ConfigurationBatch 的唯一 codec 构造并移交；本层只拥有传输，服务端仍完整验证正文。
    /// # Errors
    /// 正文/配置非法、事务冲突、执行服务拒绝、资源不足或服务不可用。
    pub fn prepare_configuration(
        &self,
        encoded: Vec<u8>,
    ) -> Result<super::PreparedConfiguration, Status> {
        let content = Vmo::from_bytes(&encoded)?;
        let mut request = Writer::request(PREPARE_CONFIGURATION);
        request.try_reserve(8)?;
        request.u64(encoded.len() as u64);
        drop(encoded);
        let mut handles = Vec::new();
        handles
            .try_reserve_exact(1)
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        handles.push(content.into_handle());
        self.call_prepared(&request, handles)
    }

    /// 把一块网卡安装进默认网络，返回接口序号与接口的 lifetime channel（接口移除时 `PEER_CLOSED`）。
    ///
    /// # Errors
    ///
    /// 服务拒绝该设备或已终止。
    pub fn install_device(
        &self,
        device_id: u64,
        description: &[u8],
        handles: Vec<Handle>,
    ) -> Result<(u32, Channel), Status> {
        let mut writer = Writer::request(INSTALL_DEVICE);
        writer.try_reserve(
            12usize
                .checked_add(description.len())
                .ok_or(native_abi::ERR_OUT_OF_RANGE)?,
        )?;
        writer.u64(device_id).bytes(description);
        let mut reply = call(&self.channel, &writer, handles)?;
        if reply.handles.len() != 1 {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        let (mut reader, handles) = reply.split();
        let index = reader.u32()?;
        reader.finish()?;
        let lifetime = Channel::from_handle(service_rpc::take_handle(handles, 0)?);
        Ok((index, lifetime))
    }
}
