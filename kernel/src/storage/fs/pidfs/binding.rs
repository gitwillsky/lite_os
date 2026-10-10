//! PIDFS 对实际 Task association 的只读 binding；不复制 Task 生命周期或授权策略。
use super::super::device::DeviceError;
use super::PidFdSource;
use crate::namespace::PidIdentity;
use alloc::sync::Arc;

/// actual graph association 的只读 poll 投影；不在 pidfs 保存 Running/Exited cache。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PidFdPoll {
    Running,
    Exited,
    Reaped,
}
/// task 注入实际 PID association 操作；实现只 pin PID/source，不 pin Task/Process/MM/files。
pub(crate) trait PidFdTarget: Send + Sync {
    fn identity(&self) -> &Arc<PidIdentity>;
    fn source(&self) -> &Arc<PidFdSource>;
    fn poll(&self) -> PidFdPoll;
    fn namespace(
        &self,
        selector: Option<crate::namespace::NamespaceSelector>,
        has_argument: bool,
    ) -> Result<crate::namespace::NamespaceReference, DeviceError>;
    /// caller view 中的 native PID/credential 字段；只有查询快照，不保存 live 状态或凭据镜像。
    fn information(&self, view: &crate::namespace::PidNamespace) -> Option<[u32; 11]>;
}
