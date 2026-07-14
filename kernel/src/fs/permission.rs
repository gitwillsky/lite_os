use alloc::{sync::Arc, vec::Vec};

use super::{FileSystemError, InodeMetadata};

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

/// @description 新 inode 的 mode 与 owner，由 VFS 在 parent policy 后一次决定。
#[derive(Clone, Copy)]
pub(crate) struct CreateMetadata {
    pub(crate) mode: u32,
    pub(crate) uid: u32,
    pub(crate) gid: u32,
}
