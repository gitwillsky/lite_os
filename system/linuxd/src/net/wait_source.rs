//! Socket 只输出实际 edge source；I/O level 留在具体 backend owner。
use crate::ipc::Pipe;
use alloc::sync::Arc;
/// 唯一 socket notification capability；没有与 byte transport 混用的 data 分支。
pub(crate) struct SocketWaitSource(Arc<Pipe>);
impl SocketWaitSource {
    pub(super) fn notification(pipe: Arc<Pipe>) -> Self {
        Self(pipe)
    }
    /// File readiness 与 syscall transient registry 消费同一实际 notification Pipe。
    pub(crate) fn into_notification(self) -> Arc<Pipe> {
        self.0
    }
}
