//! 授权来源能力：固定连接身份、完整正文替换和 peer-close 撤回，不携带调用者自选来源 ID。
use super::*;
use native_runtime::Vmo;
use router_protocol::routing::Batch;
use router_protocol::tracking::{
    self, OwnedSnapshot, Query,
    installation::{self, Installation, Outcome},
};
const VERSION: u32 = 3;
const GRANT_VERSION: u32 = 4;
const PREPARE_VERSION: u32 = 4;
const PREPARE: u32 = 1;
const TRACK: u32 = 2;
const WATCH_ROUTES: u32 = 3;

/// 配置 owner 授予生产者的范围；默认 IPv4 main 表，可显式增加 IPv6；正文不能扩大授权。
#[derive(Clone, Copy)]
pub struct Grant {
    /// 安装后用于观测的协议标记，不参与权限判断。
    pub protocol: u8,
    /// 来源可声明的最小偏好；0 留给接口事实，不能授予。
    pub minimum_preference: u32,
    /// 明确允许本来源替换 IPv6；缺省 IPv4 权限不能由正文提升。
    pub ipv6_unicast: bool,
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
        if request.u32()? != GRANT_VERSION {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        let grant = Self {
            protocol: request.u8()?,
            minimum_preference: request.u32()?,
            ipv6_unicast: match request.u8()? {
                0 => false,
                1 => true,
                _ => return Err(native_abi::ERR_INVALID_ARGS),
            },
        };
        grant.validate()?;
        Ok(grant)
    }
    pub(super) fn encode(self, request: &mut Writer) {
        request
            .u32(GRANT_VERSION)
            .u8(self.protocol)
            .u32(self.minimum_preference)
            .u8(u8::from(self.ipv6_unicast));
    }
}

/// 已获授权的来源服务；身份与授权由绑定此实例的能力决定。
pub trait SourceService: Send + Sync {
    /// 订阅每次已确认图发表；递归证明可能变化而立即出口不变，不能只比较普通 NHT 结果。
    /// # Errors
    /// 资源不足或服务不可用。
    fn watch_routes(&self) -> Result<(Channel, native_runtime::Event), Status>;
    /// 在本来源的授权观察范围派生独立跟踪会话；不扩大写权限。
    /// # Errors
    /// 查询非法、资源不足或实例终止。
    fn track(
        &self,
        queries: Vec<router_protocol::tracking::Query>,
    ) -> Result<(Channel, native_runtime::Event), Status>;

    /// 预备本族完整来源替换，保留另一族，并用同一候选图查询和验证安装；失败依赖随事务提交。
    /// # Returns
    /// 来源身份、事务能力和冻结结果；尚未激活或发表来源。
    /// # Errors
    /// 格式/授权非法、事务冲突、资源不足或执行失败；失败不发表新来源。
    fn prepare(
        &self,
        batch: Batch<'_>,
        queries: Vec<Query>,
        checks: Vec<Installation>,
    ) -> Result<SourcePreparation, Status>;
}

/// 已由 RIB 预备的来源事务与结果，移交前不需要再分配回复能力。
pub struct SourcePreparation {
    /// 服务授予的来源身份。
    pub source: u64,
    /// 同一候选的激活、确认与撤销能力。
    pub transaction: Channel,
    /// 只读查询结果及其逻辑长度。
    pub snapshot: (Vmo, u64),
    /// 只读安装检查结果及其逻辑长度。
    pub outcomes: (Vmo, u64),
    /// 确认会替换本来源本族的网关复探义务。
    pub recovery_changed: bool,
}

/// 分派来源预备版本 4：来源、查询和安装检查各一个 VMO；独立跟踪沿原协议。
/// # Errors
/// 未知操作、畸形正文/句柄、越权、并发事务或执行失败；副作用前完整检查 framing。
pub fn dispatch_source(
    service: &dyn SourceService,
    ordinal: u32,
    request: &mut Reader<'_>,
    mut handles: Vec<Handle>,
    reply: &mut Writer,
) -> Result<Vec<Handle>, Status> {
    if ordinal == WATCH_ROUTES {
        if request.u32()? != VERSION {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        request.finish()?;
        if !handles.is_empty() {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        return super::tracking::returned(|| service.watch_routes());
    }
    if ordinal == TRACK {
        return super::tracking::returned(|| {
            service.track(super::tracking::queries(request, handles)?)
        });
    }
    if ordinal != PREPARE {
        return Err(native_abi::ERR_NOT_SUPPORTED);
    }
    if request.u32()? != PREPARE_VERSION {
        return Err(native_abi::ERR_INVALID_ARGS);
    }
    let length = request.u64()?;
    let query_length = request.u64()?;
    let checks_length = request.u64()?;
    request.finish()?;
    if handles.len() != 3 {
        return Err(native_abi::ERR_INVALID_ARGS);
    }
    let bytes = Vmo::from_handle(service_rpc::take_handle(&mut handles, 0)?).read_bytes(length)?;
    let query_bytes =
        Vmo::from_handle(service_rpc::take_handle(&mut handles, 1)?).read_bytes(query_length)?;
    let check_bytes =
        Vmo::from_handle(service_rpc::take_handle(&mut handles, 2)?).read_bytes(checks_length)?;
    let queries = tracking::decode_queries(&query_bytes).map_err(super::tracking::status)?;
    let checks = installation::decode(&check_bytes).map_err(super::tracking::status)?;
    drop(query_bytes);
    drop(check_bytes);
    let batch = Batch::parse(&bytes).map_err(|_| native_abi::ERR_INVALID_ARGS)?;
    // 先准备回复容量；返回后只移动能力，不能在预备成功后再因容器增长丢失唯一事务。
    reply.try_reserve(25)?;
    let mut returned = Vec::new();
    returned
        .try_reserve_exact(3)
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    let prepared = service.prepare(batch, queries, checks)?;
    reply
        .u64(prepared.source)
        .u64(prepared.snapshot.1)
        .u64(prepared.outcomes.1)
        .u8(u8::from(prepared.recovery_changed));
    returned.push(prepared.transaction.into_handle());
    returned.push(prepared.snapshot.0.into_handle());
    returned.push(prepared.outcomes.0.into_handle());
    Ok(returned)
}

/// 独立路由来源的客户端；最后一个客户端句柄关闭后服务端撤回其全部声明。
pub struct RoutingSource(Channel);

/// 一份来源的在途事务与同批候选观察；不能脱离派生它的来源寿命或混用另一批的证明。
/// 未确认时关闭能力由服务端撤销，不在析构中追加 RPC。
#[must_use = "来源预备尚未发布；检查后激活并确认，或撤销/丢弃"]
pub struct PreparedSource<'a> {
    transaction: crate::PreparedConfiguration,
    snapshot: OwnedSnapshot,
    outcomes: Vec<Outcome>,
    recovery_changed: bool,
    id: u64,
    // OWNER: 静态借用阻止正常客户端先丢弃来源再确认派生事务；远端退出仍由 RIB 登记复核。
    source: core::marker::PhantomData<&'a RoutingSource>,
}
impl PreparedSource<'_> {
    /// 借用与输入同序的安装判断，仅对本事务的候选图有效。
    pub fn outcomes(&self) -> &[Outcome] {
        &self.outcomes
    }
    /// 本批确认会增减当前来源的复探义务，即使选中路由未变。
    pub fn recovery_changed(&self) -> bool {
        self.recovery_changed
    }
    /// 提供者赋予的真实来源身份，供候选路由归属核对；不能用于扩大能力或指定安装身份。
    pub fn source_id(&self) -> u64 {
        self.id
    }
    /// 借用同批候选图的冻结查询；不是已发表状态，代际不能跨事务比较或充当 BGP 安装准入证明。
    pub fn snapshot(&self) -> tracking::Snapshot<'_> {
        self.snapshot.view()
    }
    /// 激活这份候选，确认前可撤销；原已确认来源尚未发表。
    /// # Errors
    /// 来源已关闭、设备消失、事务无效或执行服务不可用。
    pub fn activate(&self) -> Result<(), crate::SocketError> {
        self.transaction.activate()
    }
    /// 确认执行并消费整批观察/事务；只有成功后调用者才可提交自己的协议选择。
    /// # Errors
    /// 未激活、来源已关闭、事务无效或服务失效；传输失败不能证明未确认。
    pub fn confirm(self) -> Result<(), crate::SocketError> {
        self.transaction.confirm()
    }
    /// 同步撤销并消费整批观察/事务。
    /// # Errors
    /// 设备消失、事务无效或执行服务不可用。
    pub fn abort(self) -> Result<(), crate::SocketError> {
        self.transaction.abort()
    }
}
impl RoutingSource {
    /// 订阅全部已确认图发表，供依赖完整递归图的协议重算；不由未变化的立即出口压制通知。
    /// # Errors
    /// 资源不足、回复畸形或提供者退出。
    pub fn watch_routes(&self) -> Result<super::Tracking, Status> {
        let mut request = Writer::request(WATCH_ROUTES);
        request.try_reserve(4)?;
        request.u32(VERSION);
        super::Tracking::from_reply(service_rpc::call(&self.0, &request, Vec::new())?)
    }
    /// 派生同一默认实例双栈范围的独立只读跟踪会话。
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
    /// 预备唯一 Batch codec 的完整来源；同时观察新来源参与求解后的候选图。
    /// # Parameters
    /// `queries` 沿既有跟踪格式，保持顺序；返回的是本次候选查询结果，不是已发表状态或每条 BGP 候选的安装证明。
    /// `checks` 是本族待安装的完整候选检查；确认后替换本族复探义务，空集合清除。
    /// # Returns
    /// 绑定本来源寿命、真实身份和冻结观察的事务。检查后激活/确认，或撤销/关闭；期间不得继续替换同一 RIB。
    /// # Errors
    /// 格式/授权非法、事务冲突、资源不足、回复畸形或实例退出；接收失败关闭临时能力并触发撤销。
    pub fn prepare(
        &self,
        encoded: &[u8],
        queries: &[Query],
        checks: &[Installation],
    ) -> Result<PreparedSource<'_>, Status> {
        let query_bytes = tracking::encode_queries(queries).map_err(super::tracking::status)?;
        let check_bytes = installation::encode(checks).map_err(super::tracking::status)?;
        let content = Vmo::from_bytes(encoded)?;
        let query_content = Vmo::from_bytes(&query_bytes)?;
        let check_content = Vmo::from_bytes(&check_bytes)?;
        let mut request = Writer::request(PREPARE);
        request.try_reserve(28)?;
        request
            .u32(PREPARE_VERSION)
            .u64(encoded.len() as u64)
            .u64(query_bytes.len() as u64)
            .u64(check_bytes.len() as u64);
        drop(query_bytes);
        drop(check_bytes);
        let mut handles = Vec::new();
        handles
            .try_reserve_exact(3)
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        handles.push(content.into_handle());
        handles.push(query_content.into_handle());
        handles.push(check_content.into_handle());
        let mut reply = service_rpc::call(&self.0, &request, handles)?;
        let mut reader = reply.reader();
        let id = reader.u64()?;
        let length = reader.u64()?;
        let outcomes_length = reader.u64()?;
        let recovery_changed = match reader.u8()? {
            0 => false,
            1 => true,
            _ => return Err(native_abi::ERR_INVALID_ARGS),
        };
        reader.finish()?;
        if id <= 1 || reply.handles.len() != 3 {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        let transaction = crate::PreparedConfiguration::new(Channel::from_handle(
            service_rpc::take_handle(&mut reply.handles, 0)?,
        ));
        let bytes = Vmo::from_handle(service_rpc::take_handle(&mut reply.handles, 1)?)
            .read_bytes(length)?;
        let snapshot = OwnedSnapshot::capture(bytes).map_err(super::tracking::status)?;
        if !snapshot
            .view()
            .records()
            .map(|record| record.query)
            .eq(queries.iter().copied())
        {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        let bytes = Vmo::from_handle(service_rpc::take_handle(&mut reply.handles, 2)?)
            .read_bytes(outcomes_length)?;
        let outcomes =
            installation::decode_outcomes(&bytes, id, snapshot.view().generation(), checks.len())
                .map_err(super::tracking::status)?;
        Ok(PreparedSource {
            transaction,
            snapshot,
            outcomes,
            recovery_changed,
            id,
            source: core::marker::PhantomData,
        })
    }

    /// 沿同一预备/激活/确认路径完整替换来源；空 Batch 撤回，不保留独立的直接替换线格式。
    /// # Errors
    /// 格式/授权非法、事务冲突、资源不足或服务失效；传输失败可能已经提交，不能假定未执行。
    pub fn replace(&self, encoded: Vec<u8>) -> Result<(), Status> {
        let status = |error| match error {
            crate::SocketError::NoMemory => native_abi::ERR_NO_MEMORY,
            _ => native_abi::ERR_BAD_STATE,
        };
        let transaction = self.prepare(&encoded, &[], &[])?;
        drop(encoded);
        if let Err(error) = transaction.activate() {
            let _ = transaction.abort();
            return Err(status(error));
        }
        transaction.confirm().map_err(status)
    }
}
