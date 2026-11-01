//! System V IPC 权限从本次调用的 effective credentials 判断，IDs 始终为 global kuid/kgid。
use super::IpcError;
use crate::namespace::{NamespaceAuthority, UserNamespace};
use alloc::{sync::Arc, vec::Vec};
use syscall_abi::capability::Capability;

/// 一次 IPC operation 的不可变凭据输入；不保留 Task 或第二份可变 credential owner。
pub(crate) struct IpcAccess {
    pub(crate) authority: NamespaceAuthority,
    /// Linux in_group_p 使用 fsgid；creator gid 和 capability authority 仍使用 egid。
    pub(crate) filesystem_gid: u32,
    pub(crate) groups: Option<Arc<Vec<u32>>>,
}

/// 单个实际 IPC 对象的 canonical ipc_perm；queue lock 串行控制与使用权限。
#[derive(Clone, Copy)]
pub(crate) struct IpcPermission {
    pub(crate) key: i32,
    pub(crate) uid: u32,
    pub(crate) gid: u32,
    pub(crate) creator_uid: u32,
    pub(crate) creator_gid: u32,
    pub(crate) mode: u16,
}
impl IpcPermission {
    pub(super) fn new(key: i32, mode: u16, access: &IpcAccess) -> Self {
        Self {
            key,
            uid: access.authority.effective_uid,
            gid: access.authority.effective_gid,
            creator_uid: access.authority.effective_uid,
            creator_gid: access.authority.effective_gid,
            mode: mode & 0o777,
        }
    }
    /// 任意类的 read/write/execute 请求合并后按 uid/group 类校验；IPC_OWNER 授权目标 owning UserNS。
    pub(super) fn require(
        &self,
        access: &IpcAccess,
        user: &Arc<UserNamespace>,
        requested: u16,
    ) -> Result<(), IpcError> {
        let requested = (requested | requested >> 3 | requested >> 6) & 7;
        let uid = access.authority.effective_uid;
        let in_group = |gid| {
            access.filesystem_gid == gid
                || access
                    .groups
                    .as_ref()
                    .is_some_and(|groups| groups.contains(&gid))
        };
        let shift = if uid == self.uid || uid == self.creator_uid {
            6
        } else if in_group(self.gid) || in_group(self.creator_gid) {
            3
        } else {
            0
        };
        if (self.mode >> shift) & requested == requested
            || access.authority.capable(user, Capability::IPC_OWNER)
        {
            Ok(())
        } else {
            Err(IpcError::Access)
        }
    }
    pub(super) fn require_control(
        &self,
        access: &IpcAccess,
        user: &Arc<UserNamespace>,
    ) -> Result<(), IpcError> {
        if access.authority.effective_uid == self.uid
            || access.authority.effective_uid == self.creator_uid
            || access.authority.capable(user, Capability::SYS_ADMIN)
        {
            Ok(())
        } else {
            Err(IpcError::Permission)
        }
    }
}
