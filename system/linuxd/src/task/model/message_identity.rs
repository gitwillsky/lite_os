//! 一次实际 credential owner 快照；socket 消息不能混用不同 publication 的 UID/GID/capability。
use crate::namespace::{NamespaceAuthority, UserNamespace};
use syscall_abi::capability::Capability;

/// Linux SCM_CREDENTIALS 的映射或身份声明拒绝；不修改 credential owner。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MessageIdentityError {
    Invalid,
    Permission,
}

/// 只在调用开始时捕获的不可变消息身份输入；不是 live credential/cache 的第二个 owner。
pub(crate) struct MessageIdentity {
    authority: NamespaceAuthority,
    uids: [u32; 3],
    gids: [u32; 3],
}
impl MessageIdentity {
    /// 唯一 credential owner 在同一 guard 下准备完整 snapshot。
    pub(super) fn new(authority: NamespaceAuthority, uids: [u32; 3], gids: [u32; 3]) -> Self {
        Self {
            authority,
            uids,
            gids,
        }
    }
    /// 投影 real 或 effective canonical IDs；SO_PEERCRED 与自动消息凭据选择不同的原生身份。
    pub(crate) fn ids(&self, effective: bool) -> [u32; 2] {
        let index = usize::from(effective);
        [self.uids[index], self.gids[index]]
    }
    /// 在 sender UserNS 映射请求 UID/GID，然后验证 native PID 与三组身份/capability。
    ///
    /// # Parameters
    /// - `uid` / `gid`: userspace ucred 中的 namespace-local IDs。
    /// - `own_pid`: 请求 PID 是否等于 caller 在实际 active PID namespace 中的 TGID。
    /// - `pid_owner`: actual active PID namespace 的 owning UserNS。
    ///
    /// # Returns
    /// 已验证的 canonical UID/GID；PID observer 查询由 namespace 唯一 association 完成。
    ///
    /// # Errors
    /// 先返回无映射 Invalid；PID/UID/GID 声明超出实际权限时为 Permission。
    pub(crate) fn validate_claim(
        &self,
        uid: u32,
        gid: u32,
        own_pid: bool,
        pid_owner: &alloc::sync::Arc<UserNamespace>,
    ) -> Result<[u32; 2], MessageIdentityError> {
        let namespace = &self.authority.namespace;
        let uid = namespace
            .to_kernel(true, uid)
            .ok_or(MessageIdentityError::Invalid)?;
        let gid = namespace
            .to_kernel(false, gid)
            .ok_or(MessageIdentityError::Invalid)?;
        if !(own_pid || self.authority.capable(pid_owner, Capability::SYS_ADMIN))
            || !(self.uids.contains(&uid) || self.authority.capable(namespace, Capability::SETUID))
            || !(self.gids.contains(&gid) || self.authority.capable(namespace, Capability::SETGID))
        {
            return Err(MessageIdentityError::Permission);
        }
        Ok([uid, gid])
    }
}
