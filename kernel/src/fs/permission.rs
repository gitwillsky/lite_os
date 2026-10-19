use alloc::{sync::Arc, vec::Vec};

use syscall_abi::capability::{Capability, VALID_MASK};

use super::{FileSystemError, InodeMetadata, InodeType};

/// VFS permission evaluator 消费的不可变调用身份；凭据状态由 calling Thread 独占。
#[derive(Clone)]
pub(crate) struct AccessIdentity {
    uid: u32,
    gid: u32,
    groups: Option<Arc<Vec<u32>>>,
    capabilities: u64,
    user_namespace: Arc<crate::namespace::UserNamespace>,
    // SNAPSHOT: capability ancestry 用 EUID；inode DAC 则用 FSUID/real UID，二者不能混用。
    capability_uid: u32,
}

impl AccessIdentity {
    /// 构造一次 real/effective identity 快照。
    ///
    /// # Parameters
    ///
    /// - `uid`: 用于本次检查的用户 ID。
    /// - `gid`: 用于本次检查的主组 ID。
    /// - `capability_uid`: namespace capability ancestry 使用的 calling EUID；不得以 FSUID 替代。
    /// - `groups`: calling Thread 的不可变、有序 supplementary groups；唯一 credential producer
    ///   在发布前排序，空集合不分配。此快照不执行分配或再次排序。
    ///
    /// # Returns
    ///
    /// 不持有 Thread credential lock 的权限输入。
    pub(crate) fn new(
        uid: u32,
        gid: u32,
        groups: Option<Arc<Vec<u32>>>,
        user_namespace: Arc<crate::namespace::UserNamespace>,
        capabilities: u64,
        capability_uid: u32,
    ) -> Self {
        Self {
            uid,
            gid,
            groups,
            capabilities,
            user_namespace,
            capability_uid,
        }
    }

    pub(crate) fn root() -> Self {
        Self::new(0, 0, None, crate::namespace::initial_user(), VALID_MASK, 0)
    }

    /// 创建 filesystem 的 immutable user namespace；FSUID/GID 默认 owner 单独读取。
    pub(crate) fn user_namespace(&self) -> Arc<crate::namespace::UserNamespace> {
        self.user_namespace.clone()
    }

    /// scoped resource capability 不借 FSUID 的 owner 特例；始终用捕获的 calling EUID。
    pub(crate) fn namespace_capable(
        &self,
        namespace: &Arc<crate::namespace::UserNamespace>,
        capability: Capability,
    ) -> bool {
        crate::namespace::NamespaceAuthority {
            namespace: self.user_namespace.clone(),
            effective_uid: self.capability_uid,
            effective_gid: self.gid,
            effective_capabilities: self.capabilities,
        }
        .capable(namespace, capability)
    }
    /// capable() 的全局 scope，例如内容写入的 CAP_FSETID；child user namespace 不授权它。
    pub(crate) fn has_capability(&self, capability: Capability) -> bool {
        self.user_namespace.is_initial() && self.capabilities & capability.mask() != 0
    }

    /// 固定 capable_wrt_inode_uidgid：两个 inode owner IDs 都映射进 calling user namespace。
    pub(crate) fn inode_capability(&self, uid: u32, gid: u32, capability: Capability) -> bool {
        self.capabilities & capability.mask() != 0
            && self.user_namespace.map_kernel_id(true, uid).is_some()
            && self.user_namespace.map_kernel_id(false, gid).is_some()
    }

    /// 固定 inode_owner_or_capable：owner 相等，或 CAP_FOWNER 与 inode UID 映射成立。
    pub(crate) fn owns_inode(&self, uid: u32) -> bool {
        self.uid == uid
            || (self.capabilities & Capability::FOWNER.mask() != 0
                && self.user_namespace.map_kernel_id(true, uid).is_some())
    }

    /// 已授权的内容 mutation 要清除的 set-ID 位；ATTR_FORCE 不要求 writer 是 inode owner。
    pub(crate) fn write_kill_mask(&self, kind: InodeType, mode: u16, gid: u32) -> u16 {
        if kind != InodeType::File || self.has_capability(Capability::FSETID) {
            return 0;
        }
        let suid = mode & 0o4000;
        let sgid = if mode & 0o0010 != 0 || !self.in_group(gid) {
            mode & 0o2000
        } else {
            0
        };
        suid | sgid
    }

    /// 返回本次检查使用的 UID。
    ///
    /// # Returns
    ///
    /// real 或 effective UID，由 calling Thread snapshot 构造者决定。
    pub(crate) fn uid(&self) -> u32 {
        self.uid
    }
    /// 调用者实际EUID；全局proc sysctl使用此权限身份，不能借FSUID或inode DAC capability。
    pub(crate) fn effective_uid(&self) -> u32 {
        self.capability_uid
    }

    /// 返回本次检查使用的 primary GID。
    ///
    /// # Returns
    ///
    /// real 或 effective GID，由 calling Thread snapshot 构造者决定。
    pub(crate) fn gid(&self) -> u32 {
        self.gid
    }

    /// 判断 GID 是否属于 primary 或 supplementary groups。
    ///
    /// # Parameters
    ///
    /// - `gid`: 待查询的组 ID。
    ///
    /// # Returns
    ///
    /// membership 存在时为 true。
    pub(crate) fn in_group(&self, gid: u32) -> bool {
        self.gid == gid
            || self
                .groups
                .as_ref()
                .is_some_and(|groups| groups.binary_search(&gid).is_ok())
    }

    /// sticky directory 的 entry owner policy；UID 0 本身不授予 FOWNER。
    pub(crate) fn may_remove_entry(
        &self,
        directory: InodeMetadata,
        target_uid: u32,
        target_gid: u32,
    ) -> bool {
        directory.mode & 0o1000 == 0
            || self.uid == directory.uid
            || self.uid == target_uid
            || self.inode_capability(target_uid, target_gid, Capability::FOWNER)
    }

    /// 按 owner/group/other 与 DAC capability execute 规则判断 inode access。
    ///
    /// # Parameters
    ///
    /// - `metadata`: inode 的同一时刻元数据快照。
    /// - `requested`: Linux R/W/X bit mask。
    ///
    /// # Returns
    ///
    /// 所有请求 bit 均允许时为 true。
    pub(crate) fn permits(&self, metadata: InodeMetadata, requested: u8) -> bool {
        self.permits_owner(
            &OwnerModeState::new(
                metadata.kind,
                metadata.mode as u16,
                metadata.uid,
                metadata.gid,
            ),
            requested,
        )
    }

    fn permits_owner(&self, owner: &OwnerModeState, requested: u8) -> bool {
        let granted = if self.uid == owner.uid {
            (owner.mode >> 6) & 7
        } else if self.in_group(owner.gid) {
            (owner.mode >> 3) & 7
        } else {
            owner.mode & 7
        } as u8;
        if granted & requested == requested {
            return true;
        }
        if self.inode_capability(owner.uid, owner.gid, Capability::DAC_OVERRIDE)
            && (owner.kind == InodeType::Directory || requested & 1 == 0 || owner.mode & 0o111 != 0)
        {
            return true;
        }
        self.inode_capability(owner.uid, owner.gid, Capability::DAC_READ_SEARCH)
            && if owner.kind == InodeType::Directory {
                requested & 2 == 0
            } else {
                requested == 4
            }
    }

    /// inode mutation gate内的同一live owner facts使用既有DAC算法，不递归取得inode lock。
    pub(super) fn require_owner(
        &self,
        owner: &OwnerModeState,
        requested: u8,
    ) -> Result<(), FileSystemError> {
        self.permits_owner(owner, requested)
            .then_some(())
            .ok_or(FileSystemError::AccessDenied)
    }

    /// 将 permission predicate 转换为 VFS AccessDenied。
    ///
    /// # Parameters
    ///
    /// - `metadata`: inode 元数据快照。
    /// - `requested`: Linux R/W/X bit mask。
    ///
    /// # Returns
    ///
    /// 允许为 Ok，否则为 AccessDenied。
    pub(crate) fn require(
        &self,
        metadata: InodeMetadata,
        requested: u8,
    ) -> Result<(), FileSystemError> {
        self.permits(metadata, requested)
            .then_some(())
            .ok_or(FileSystemError::AccessDenied)
    }
}

/// chmod/chown 的语义请求；VFS permission evaluator 在 live inode state 上唯一授权。
pub(crate) struct OwnerModeChange {
    identity: AccessIdentity,
    operation: OwnerModeOperation,
}

#[derive(Clone, Copy)]
enum OwnerModeOperation {
    Chmod { mode: u32 },
    Chown { uid: Option<u32>, gid: Option<u32> },
}

/// filesystem mutation owner 提供的一次 live owner/mode state；授权结果仍由同一值返回。
pub(super) struct OwnerModeState {
    kind: InodeType,
    mode: u16,
    uid: u32,
    gid: u32,
}

impl OwnerModeState {
    pub(super) const fn kind(&self) -> InodeType {
        self.kind
    }
    pub(super) const fn new(kind: InodeType, mode: u16, uid: u32, gid: u32) -> Self {
        Self {
            kind,
            mode,
            uid,
            gid,
        }
    }

    pub(super) const fn mode(&self) -> u16 {
        self.mode
    }

    pub(super) const fn uid(&self) -> u32 {
        self.uid
    }

    pub(super) const fn gid(&self) -> u32 {
        self.gid
    }
}

impl OwnerModeChange {
    /// 固定chown_common给所有非directory设置ATTR_KILL_PRIV，UID/GID均为-1也不能绕过。
    pub(super) fn kills_privileges(&self, kind: InodeType) -> bool {
        kind != InodeType::Directory && matches!(self.operation, OwnerModeOperation::Chown { .. })
    }
    /// proc 的 setattr policy 拒绝 ATTR_MODE；chown 的授权仍由同一 evaluator 负责。
    pub(super) fn changes_mode(&self) -> bool {
        matches!(self.operation, OwnerModeOperation::Chmod { .. })
    }

    /// 构造已脱离 Process lock 的 chmod 请求；授权延迟到 filesystem live-state lock 内。
    pub(crate) fn chmod(identity: AccessIdentity, mode: u32) -> Self {
        Self {
            identity,
            operation: OwnerModeOperation::Chmod { mode },
        }
    }

    /// 构造已把 `-1` ABI sentinel 解码为 None 的 chown 请求。
    pub(crate) fn chown(identity: AccessIdentity, uid: Option<u32>, gid: Option<u32>) -> Self {
        Self {
            identity,
            operation: OwnerModeOperation::Chown { uid, gid },
        }
    }

    /// 对 immutable/read-only inode snapshot 保留与 writable inode 相同的权限错误顺序。
    pub(super) fn authorize_metadata(self, metadata: InodeMetadata) -> Result<(), FileSystemError> {
        let mode = u16::try_from(metadata.mode).map_err(|_| FileSystemError::InvalidOperation)?;
        self.authorize(OwnerModeState::new(
            metadata.kind,
            mode,
            metadata.uid,
            metadata.gid,
        ))
        .map(|_| ())
    }

    /// 对 mutation owner 提供的 live state 唯一执行 owner/group/set-ID policy。
    ///
    /// # Parameters
    ///
    /// - `current`: mutation lock 下读取的同一 inode mode/UID/GID。
    ///
    /// # Returns
    ///
    /// 已授权的完整 replacement state；无权限返回 PermissionDenied。
    pub(super) fn authorize(
        &self,
        mut current: OwnerModeState,
    ) -> Result<OwnerModeState, FileSystemError> {
        // 借用 snapshot；它可能是已退休 credential groups 的最后一个 Arc，
        // 只能由调用者在 live-state guard 释放后析构，不能在此消费它。
        let identity = &self.identity;
        let operation = self.operation;
        match operation {
            OwnerModeOperation::Chmod { mode } => {
                if !identity.owns_inode(current.uid) {
                    return Err(FileSystemError::PermissionDenied);
                }
                let mut mode = mode as u16 & 0o7777;
                if !identity.has_capability(Capability::FSETID) && !identity.in_group(current.gid) {
                    mode &= !0o2000;
                }
                current.mode = current.mode & !0o7777 | mode;
            }
            OwnerModeOperation::Chown { uid, gid } => {
                let privileged =
                    identity.inode_capability(current.uid, current.gid, Capability::CHOWN);
                if let Some(uid) = uid
                    && !privileged
                    && (identity.uid() != current.uid || uid != current.uid)
                {
                    return Err(FileSystemError::PermissionDenied);
                }
                if let Some(gid) = gid
                    && !privileged
                    && (identity.uid() != current.uid
                        || (gid != current.gid && !identity.in_group(gid)))
                {
                    return Err(FileSystemError::PermissionDenied);
                }

                // Linux chown_common applies ATTR_KILL_SUID/SGID to every
                // non-directory, including a sentinel-only request. Root
                // models the capability bypass; group-execute still forces
                // SGID removal for compatibility with Linux setattr policy.
                if current.kind != InodeType::Directory {
                    let original_mode = current.mode;
                    current.mode &= !0o4000;
                    if current.mode & 0o2010 == 0o2010
                        || (!identity.has_capability(Capability::FSETID)
                            && !identity.in_group(current.gid))
                    {
                        current.mode &= !0o2000;
                    }
                    // notify_change materializes an actual set-ID drop as
                    // ATTR_MODE, so ext4 setattr_prepare requires owner/root
                    // even when both UID/GID arguments were sentinels.
                    if current.mode != original_mode && !identity.owns_inode(current.uid) {
                        return Err(FileSystemError::PermissionDenied);
                    }
                }
                if let Some(uid) = uid {
                    current.uid = uid;
                }
                if let Some(gid) = gid {
                    current.gid = gid;
                }
            }
        }
        Ok(current)
    }
}

/// 新 inode 的 mode 与 owner，由 VFS 在 parent policy 后一次决定。
#[derive(Clone, Copy)]
pub(crate) struct CreateMetadata {
    pub(crate) mode: u32,
    pub(crate) uid: u32,
    pub(crate) gid: u32,
}
