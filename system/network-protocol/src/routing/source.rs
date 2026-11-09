//! 授权来源能力：固定连接身份、完整正文替换和 peer-close 撤回，不携带调用者自选来源 ID。
use super::*;
use native_runtime::Vmo;
use router_protocol::routing::Batch;
const VERSION: u32 = 1;
const REPLACE: u32 = 1;
const TRACK: u32 = 2;

/// 配置 owner 授予生产者的范围；当前仅默认 IPv4 main 表，不从正文接受实例或地址族升级。
#[derive(Clone, Copy)]
pub struct Grant {
    /// 安装后用于观测的协议标记，不参与权限判断。
    pub protocol: u8,
    /// 来源可声明的最小偏好；0 留给接口事实，不能授予。
    pub minimum_preference: u32,
}
impl Grant {
    pub(super) fn validate(self) -> Result<(), Status> {
        if self.minimum_preference == 0 {
            Err(native_abi::ERR_INVALID_ARGS)
        } else {
            Ok(())
        }
    }
    pub(super) fn decode(request: &mut Reader<'_>) -> Result<Self, Status> {
        if request.u32()? != VERSION {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        let grant = Self {
            protocol: request.u8()?,
            minimum_preference: request.u32()?,
        };
        grant.validate()?;
        Ok(grant)
    }
    pub(super) fn encode(self, request: &mut Writer) {
        request
            .u32(VERSION)
            .u8(self.protocol)
            .u32(self.minimum_preference);
    }
}

/// 已获授权的来源服务；身份与授权由绑定此实例的能力决定。
pub trait SourceService: Send + Sync {
    /// 在本来源的授权观察范围派生独立跟踪会话；不扩大写权限。
    /// # Errors
    /// 查询非法、资源不足或实例终止。
    fn track(
        &self,
        queries: Vec<router_protocol::tracking::Query>,
    ) -> Result<(Channel, native_runtime::Event), Status>;

    /// 完整替换本会话声明；空集合撤回路由但保留会话，成功仅在 NET 确认后返回。
    /// # Errors
    /// 格式/授权非法、事务冲突、资源不足或执行失败；失败不发表新来源。
    fn replace(&self, batch: Batch<'_>) -> Result<(), Status>;
}

/// 分派来源版本 1：请求携带 u32 版本/u64 正文长度和一个只读 VMO。
/// # Errors
/// 未知操作、畸形正文/句柄、越权、并发事务或执行失败；副作用前完整检查 framing。
pub fn dispatch_source(
    service: &dyn SourceService,
    ordinal: u32,
    request: &mut Reader<'_>,
    mut handles: Vec<Handle>,
    _: &mut Writer,
) -> Result<Vec<Handle>, Status> {
    if ordinal == TRACK {
        return super::tracking::returned(|| {
            service.track(super::tracking::queries(request, handles)?)
        });
    }
    if ordinal != REPLACE {
        return Err(native_abi::ERR_NOT_SUPPORTED);
    }
    if request.u32()? != VERSION {
        return Err(native_abi::ERR_INVALID_ARGS);
    }
    let length = request.u64()?;
    request.finish()?;
    if handles.len() != 1 {
        return Err(native_abi::ERR_INVALID_ARGS);
    }
    let bytes = Vmo::from_handle(service_rpc::take_handle(&mut handles, 0)?).read_bytes(length)?;
    service.replace(Batch::parse(&bytes).map_err(|_| native_abi::ERR_INVALID_ARGS)?)?;
    Ok(Vec::new())
}

/// 独立路由来源的客户端；最后一个客户端句柄关闭后服务端撤回其全部声明。
pub struct RoutingSource(Channel);
impl RoutingSource {
    /// 派生同一默认 IPv4 实例范围的独立只读跟踪会话。
    /// # Errors
    /// 查询非法、资源不足、回复畸形或实例关闭。
    pub fn track(
        &self,
        queries: &[router_protocol::tracking::Query],
    ) -> Result<super::Tracking, Status> {
        let (request, handles) = super::tracking::request(TRACK, queries)?;
        super::Tracking::from_reply(service_rpc::call(&self.0, &request, handles)?)
    }

    /// 接管明确授予的来源能力，不接受来源 ID 参数。
    pub fn new(channel: Channel) -> Self {
        Self(channel)
    }
    /// 观察 RIB 退出；关闭的能力不能向另一实例重放。
    pub fn channel(&self) -> &Channel {
        &self.0
    }
    /// 移交来源能力供协议进程独占；不会创建更大权限的工厂能力。
    pub fn into_channel(self) -> Channel {
        self.0
    }
    /// 以唯一 Batch codec 的完整声明替换本来源；空 Batch 撤回。
    /// # Errors
    /// 格式/授权非法、事务冲突、资源不足或服务失效；传输失败可能已经提交，不能假定未执行。
    pub fn replace(&self, encoded: Vec<u8>) -> Result<(), Status> {
        let content = Vmo::from_bytes(&encoded)?;
        let mut request = Writer::request(REPLACE);
        request.try_reserve(12)?;
        request.u32(VERSION).u64(encoded.len() as u64);
        drop(encoded);
        let mut handles = Vec::new();
        handles
            .try_reserve_exact(1)
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        handles.push(content.into_handle());
        let reply = service_rpc::call(&self.0, &request, handles)?;
        reply.reader().finish()?;
        if !reply.handles.is_empty() {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        Ok(())
    }
}
