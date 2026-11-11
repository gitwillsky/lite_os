//! 来源确认只交换 RIB 目录，执行阶段与回滚前像唯一由 NET 事务拥有。
use super::*;
use network_protocol::{ConfigurationService, PreparedConfiguration, SocketError};

struct Candidate {
    routes: Routes,
    execution: PreparedConfiguration,
}
pub(super) struct Transaction {
    owner: Arc<Rib>,
    id: u64,
    // OWNER: 派生本租约的来源身份；确认前按原登记复核寿命，缺失会让已关闭来源被晚到确认重新发表。
    source: Option<u64>,
    // OWNER: 同一能力上的激活/终结互斥；候选与 NET 能力一起移交，避免确认后仍可误撤销。
    candidate: Mutex<Option<Candidate>>,
}
impl Transaction {
    fn new(
        owner: Arc<Rib>,
        id: u64,
        source: Option<u64>,
        routes: Routes,
        execution: PreparedConfiguration,
    ) -> Self {
        Self {
            owner,
            id,
            source,
            candidate: Mutex::new(Some(Candidate { routes, execution })),
        }
    }
    /// 为接口或来源批次提供同一事务协议；调用者已经取得唯一租约，且必须在状态锁外移交。
    /// # Errors
    /// 分配/服务线程创建失败；同一 guard 关闭 NET 能力并归还 RIB 租约。
    pub(super) fn serve(
        owner: Arc<Rib>,
        id: u64,
        source: Option<u64>,
        routes: Routes,
        execution: PreparedConfiguration,
        server: Channel,
    ) -> Result<(), Status> {
        let session = Arc::try_new(Self::new(owner, id, source, routes, execution))
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        let handler = Arc::try_new(
            move |ordinal,
                  request: &mut service_rpc::Reader<'_>,
                  handles,
                  reply: &mut service_rpc::Writer| {
                network_protocol::dispatch_configuration(
                    &*session, ordinal, request, handles, reply,
                )
            },
        )
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        service_rpc::serve(server, 1, b"rib-transaction", handler)
    }
    fn source_live(&self, state: &State) -> bool {
        self.source.is_none_or(|id| {
            state
                .sources
                .iter()
                .any(|source| source.id == id && !source.retiring)
        })
    }
    fn release(&self) {
        let mut state = self.owner.state.lock();
        if state.pending == Some(self.id) {
            state.pending = None;
            drop(state);
            self.owner
                .released
                .signal(0, signals::USER_0)
                .expect("lease release event remains valid");
        }
    }
}
impl ConfigurationService for Transaction {
    fn activate(&self) -> Result<(), SocketError> {
        let candidate = self.candidate.lock();
        let state = self.owner.state.lock();
        if !self.source_live(&state) {
            return Err(SocketError::Invalid);
        }
        candidate
            .as_ref()
            .ok_or(SocketError::Invalid)?
            .execution
            .activate()
    }
    fn confirm(&self) -> Result<(), SocketError> {
        let candidate = self.candidate.lock().take().ok_or(SocketError::Invalid)?;
        // 状态锁把来源退出与确认线性化；NET 不回调 RIB，不新增反向锁序。
        let mut state = self.owner.state.lock();
        if !self.source_live(&state) {
            drop(state);
            let _ = candidate.execution.abort();
            self.release();
            return Err(SocketError::Invalid);
        }
        confirm_execution(candidate.execution);
        let old = state.publish(candidate.routes);
        state.pending = None;
        drop(state);
        drop(old);
        self.owner
            .released
            .signal(0, signals::USER_0)
            .expect("lease release event remains valid");
        Ok(())
    }
    fn abort(&self) -> Result<(), SocketError> {
        let candidate = self.candidate.lock().take().ok_or(SocketError::Invalid)?;
        let result = candidate.execution.abort();
        self.release();
        result
    }
}

pub(super) fn confirm_execution(execution: PreparedConfiguration) {
    if let Err(error) = execution.confirm() {
        // 远端可能已确认；结束所有来源能力，不让旧 RIB 冒充与 FIB 一致。配置 owner 重放持久决定。
        native_runtime::error!("routing-rib: execution confirmation uncertain: {error:?}");
        native_runtime::task::exit_process(-1);
    }
}
impl Drop for Transaction {
    fn drop(&mut self) {
        // 关闭执行能力由 NET 自动回滚；Drop 不发 RPC，也不保留一份 NET 的阶段/逆向资源。
        self.candidate.lock().take();
        self.release();
    }
}
