//! 实际 endpoint 打开阶段：creator、backlog membership、唯一 OFD owner；观察者不是 root。
use super::{UnixPassedFile, UnixSocket};
use alloc::sync::{Arc, Weak};

/// 未发布 creator 有构造事务 root；backlog 由实际归属边连到 listener；已打开只查唯一 OFD。
/// 不把无法 upgrade 的历史 Weak 当成 creator，避免 close 后观察者令 dead cycle 永久存活。
pub(super) enum RightsOwner {
    Creator,
    Retired,
    Backlog(super::rights_graph::BacklogOwnership),
    Description(Weak<dyn UnixPassedFile>),
}
pub(super) struct RootProbe {
    pub(super) rooted: bool,
    pub(super) owner: Option<Arc<dyn UnixPassedFile>>,
}
impl RightsOwner {
    pub(super) fn replace_description(&mut self, owner: Weak<dyn UnixPassedFile>) -> Self {
        assert!(
            matches!(self, Self::Creator | Self::Backlog(_)),
            "AF_UNIX socket acquired a second OFD owner"
        );
        core::mem::replace(self, Self::Description(owner))
    }
}
impl UnixSocket {
    pub(in crate::net) fn bind_rights_owner(&self, owner: Weak<dyn UnixPassedFile>) {
        super::rights_graph::publish_description(self, owner);
    }
    /// 没有已发布 OFD 的实际 creator/backlog 与 Linux sk_socket==NULL 对应。
    pub(super) fn needs_message_credentials(&self) -> bool {
        self.pass_credentials()
            || matches!(
                *self.rights_owner.lock(),
                RightsOwner::Creator | RightsOwner::Backlog(_)
            )
    }
    pub(super) fn root_probe(&self, inflight: usize) -> RootProbe {
        match &*self.rights_owner.lock() {
            RightsOwner::Retired => RootProbe {
                rooted: false,
                owner: None,
            },
            RightsOwner::Creator => RootProbe {
                rooted: true,
                owner: None,
            },
            RightsOwner::Backlog(ownership) => {
                // capability 的存活就是实际 server→listener graph 边的存活；无独立 OFD root。
                let _ = ownership;
                RootProbe {
                    rooted: false,
                    owner: None,
                }
            }
            RightsOwner::Description(owner) => match owner.upgrade() {
                Some(owner) => {
                    let (rooted, owner) = owner.probe_external(inflight);
                    RootProbe {
                        rooted,
                        owner: Some(owner),
                    }
                }
                None => RootProbe {
                    rooted: false,
                    owner: None,
                },
            },
        }
    }
    /// 实际 backlog capability 决定是否尚未 accept；不复制 accepted flag。
    pub(super) fn is_backlog_owned(&self) -> bool {
        matches!(*self.rights_owner.lock(), RightsOwner::Backlog(_))
    }
    /// 撤销实际 root capability；旧 backlog token 必须在 socket/GRAPH guards 外释放。
    pub(super) fn retire_rights_owner(&self) {
        let retired = core::mem::replace(&mut *self.rights_owner.lock(), RightsOwner::Retired);
        drop(retired);
    }
    /// connect reservation 保活实际 listener OFD；阻止 close 与构造中的 backlog commit 竞争。
    pub(super) fn pin_description(
        &self,
    ) -> Result<Arc<dyn UnixPassedFile>, crate::net::SocketError> {
        super::rights_graph::pin_description(self)
    }
    /// server creator 在 transport/backlog publication 前消费唯一归属 capability。
    pub(super) fn bind_backlog_owner(&self, ownership: super::rights_graph::BacklogOwnership) {
        let mut owner = self.rights_owner.lock();
        assert!(
            matches!(*owner, RightsOwner::Creator),
            "backlog owner installed twice"
        );
        *owner = RightsOwner::Backlog(ownership);
    }
}
