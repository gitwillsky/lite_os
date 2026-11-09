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
    // OWNER: 同一能力上的激活/终结互斥；候选与 NET 能力一起移交，避免确认后仍可误撤销。
    candidate: Mutex<Option<Candidate>>,
}
impl Transaction {
    pub(super) fn new(
        owner: Arc<Rib>,
        id: u64,
        routes: Routes,
        execution: PreparedConfiguration,
    ) -> Self {
        Self {
            owner,
            id,
            candidate: Mutex::new(Some(Candidate { routes, execution })),
        }
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
        self.candidate
            .lock()
            .as_ref()
            .ok_or(SocketError::Invalid)?
            .execution
            .activate()
    }
    fn confirm(&self) -> Result<(), SocketError> {
        let candidate = self.candidate.lock().take().ok_or(SocketError::Invalid)?;
        confirm_execution(candidate.execution);
        let old = {
            let mut state = self.owner.state.lock();
            let old = state.publish(candidate.routes);
            state.pending = None;
            old
        };
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
