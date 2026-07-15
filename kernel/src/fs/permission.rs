use alloc::{sync::Arc, vec::Vec};

use super::{FileSystemError, InodeMetadata, InodeType};

/// @description VFS permission evaluator 消费的不可变调用身份；状态仍由 Process 独占。
#[derive(Clone)]
pub(crate) struct AccessIdentity {
    uid: u32,
    gid: u32,
    groups: Option<Arc<Vec<u32>>>,
}

impl AccessIdentity {
    /// @description 构造一次 real/effective identity 快照。
    /// @param uid 用于本次检查的用户 ID。
    /// @param gid 用于本次检查的主组 ID。
    /// @param groups Process 不可变 supplementary group snapshot；空集合不分配。
    /// @return 不持有 Process lock 的权限输入。
    pub(crate) fn new(uid: u32, gid: u32, groups: Option<Arc<Vec<u32>>>) -> Self {
        Self { uid, gid, groups }
    }

    pub(crate) fn root() -> Self {
        Self::new(0, 0, None)
    }

    /// @description 返回本次检查使用的 UID。
    /// @return real 或 effective UID，由 Process snapshot 构造者决定。
    pub(crate) fn uid(&self) -> u32 {
        self.uid
    }

    /// @description 返回本次检查使用的 primary GID。
    /// @return real 或 effective GID，由 Process snapshot 构造者决定。
    pub(crate) fn gid(&self) -> u32 {
        self.gid
    }

    /// @description 判断 GID 是否属于 primary 或 supplementary groups。
    /// @param gid 待查询的组 ID。
    /// @return membership 存在时为 true。
    pub(crate) fn in_group(&self, gid: u32) -> bool {
        self.gid == gid
            || self
                .groups
                .as_ref()
                .is_some_and(|groups| groups.contains(&gid))
    }

    /// @description 按 owner/group/other 与 root execute 规则判断 inode access。
    /// @param metadata inode 的同一时刻元数据快照。
    /// @param requested Linux R/W/X bit mask。
    /// @return 所有请求 bit 均允许时为 true。
    pub(crate) fn permits(&self, metadata: InodeMetadata, requested: u8) -> bool {
        if self.uid == 0 {
            return requested & 1 == 0 || metadata.mode & 0o111 != 0;
        }
        let granted = if self.uid == metadata.uid {
            (metadata.mode >> 6) & 7
        } else if self.in_group(metadata.gid) {
            (metadata.mode >> 3) & 7
        } else {
            metadata.mode & 7
        } as u8;
        granted & requested == requested
    }

    /// @description 将 permission predicate 转换为 VFS AccessDenied。
    /// @param metadata inode 元数据快照。
    /// @param requested Linux R/W/X bit mask。
    /// @return 允许为 Ok，否则为 AccessDenied。
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

/// @description chmod/chown 的语义请求；VFS permission evaluator 在 live inode state 上唯一授权。
pub(crate) struct OwnerModeChange {
    identity: AccessIdentity,
    operation: OwnerModeOperation,
}

enum OwnerModeOperation {
    Chmod { mode: u32 },
    Chown { uid: Option<u32>, gid: Option<u32> },
}

/// @description filesystem mutation owner 提供的一次 live owner/mode state；授权结果仍由同一值返回。
pub(super) struct OwnerModeState {
    kind: InodeType,
    mode: u16,
    uid: u32,
    gid: u32,
}

impl OwnerModeState {
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
    /// @description 构造已脱离 Process lock 的 chmod 请求；授权延迟到 filesystem live-state lock 内。
    pub(crate) fn chmod(identity: AccessIdentity, mode: u32) -> Self {
        Self {
            identity,
            operation: OwnerModeOperation::Chmod { mode },
        }
    }

    /// @description 构造已把 `-1` ABI sentinel 解码为 None 的 chown 请求。
    pub(crate) fn chown(identity: AccessIdentity, uid: Option<u32>, gid: Option<u32>) -> Self {
        Self {
            identity,
            operation: OwnerModeOperation::Chown { uid, gid },
        }
    }

    /// @description 对 immutable/read-only inode snapshot 保留与 writable inode 相同的权限错误顺序。
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

    /// @description 对 mutation owner 提供的 live state 唯一执行 owner/group/set-ID policy。
    /// @param current mutation lock 下读取的同一 inode mode/UID/GID。
    /// @return 已授权的完整 replacement state；无权限返回 PermissionDenied。
    pub(super) fn authorize(
        self,
        mut current: OwnerModeState,
    ) -> Result<OwnerModeState, FileSystemError> {
        let Self {
            identity,
            operation,
        } = self;
        match operation {
            OwnerModeOperation::Chmod { mode } => {
                if identity.uid() != 0 && identity.uid() != current.uid {
                    return Err(FileSystemError::PermissionDenied);
                }
                let mut mode = mode as u16 & 0o7777;
                if identity.uid() != 0 && !identity.in_group(current.gid) {
                    mode &= !0o2000;
                }
                current.mode = current.mode & !0o7777 | mode;
            }
            OwnerModeOperation::Chown { uid, gid } => {
                let privileged = identity.uid() == 0;
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
                        || (!privileged && !identity.in_group(current.gid))
                    {
                        current.mode &= !0o2000;
                    }
                    // notify_change materializes an actual set-ID drop as
                    // ATTR_MODE, so ext2 setattr_prepare requires owner/root
                    // even when both UID/GID arguments were sentinels.
                    if current.mode != original_mode && !privileged && identity.uid() != current.uid
                    {
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

/// @description 新 inode 的 mode 与 owner，由 VFS 在 parent policy 后一次决定。
#[derive(Clone, Copy)]
pub(crate) struct CreateMetadata {
    pub(crate) mode: u32,
    pub(crate) uid: u32,
    pub(crate) gid: u32,
}
