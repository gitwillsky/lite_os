//! 实际 peer/message PID observer 与 canonical IDs；不拥有 PID registration 或 Task。
use alloc::sync::Arc;

/// Linux AF_UNIX peer/message credential；历史 identity pin 不延迟 namespace 编号回收。
#[derive(Debug, Clone)]
pub(crate) struct UnixCredentials {
    pub(crate) pid: Arc<crate::namespace::PidIdentity>,
    pub(crate) uid: u32,
    pub(crate) gid: u32,
}

impl UnixCredentials {
    /// Native stream coalescing 比较实际 PID 对象，不能用已可复用的数字 PID。
    pub(super) fn same_as(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.pid, &other.pid) && self.uid == other.uid && self.gid == other.gid
    }
}

/// 上层一次捕获的实际 sender 与已验证显式 claim；队列仅保存最终选择的一份凭据。
pub(crate) struct UnixSendCredentials {
    pub(crate) sender: UnixCredentials,
    pub(crate) claimed: Option<UnixCredentials>,
}
impl UnixSendCredentials {
    /// 根据发送端和实际接收端 policy 选择 native skb credentials。
    pub(super) fn for_recipient(&self, required: bool) -> Option<&UnixCredentials> {
        self.claimed
            .as_ref()
            .or_else(|| required.then_some(&self.sender))
    }
}
