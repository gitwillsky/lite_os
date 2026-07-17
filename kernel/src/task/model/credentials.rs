use alloc::{sync::Arc, vec::Vec};

use super::TaskControlBlock;
use crate::fs::AccessIdentity;

const ROOT_ID: u32 = 0;
const DEFAULT_UMASK: u32 = 0o022;
const UNCHANGED_ID: u32 = u32::MAX;

/// @description Process 唯一拥有的 Linux 用户、组与文件创建 mask。
#[derive(Clone)]
pub(super) struct Credentials {
    real_uid: u32,
    effective_uid: u32,
    saved_uid: u32,
    real_gid: u32,
    effective_gid: u32,
    saved_gid: u32,
    // OWNER: immutable Arc 让 pathname permission snapshot 和 fork 只增加引用计数；
    // 如果保留 Vec clone，每个 pathname syscall 都可在 OOM 时 abort kernel。
    groups: Option<Arc<Vec<u32>>>,
    umask: u32,
}

impl TaskControlBlock {
    /// @description 取得一次权限判断身份快照。
    /// @param effective true 选择 effective ID，false 选择 real ID。
    /// @return 包含 supplementary groups 的独立快照。
    pub(crate) fn access_identity(&self, effective: bool) -> AccessIdentity {
        self.process.credentials.lock().access_identity(effective)
    }

    /// @description 读取 real/effective UID 或 GID。
    pub(crate) fn credential_id(&self, uid: bool, effective: bool) -> u32 {
        let credentials = self.process.credentials.lock();
        if uid {
            credentials.uid(effective)
        } else {
            credentials.gid(effective)
        }
    }

    /// @description 读取 real/effective/saved UID 或 GID 三元组。
    pub(crate) fn credential_res_ids(&self, uid: bool) -> [u32; 3] {
        let credentials = self.process.credentials.lock();
        if uid {
            credentials.resuids()
        } else {
            credentials.resgids()
        }
    }

    /// @description 判断 caller credentials 是否允许向 target 发送 user signal。
    pub(crate) fn may_signal(&self, target: &TaskControlBlock) -> bool {
        let sender = self.process.credentials.lock().resuids();
        let target = target.process.credentials.lock().resuids();
        sender[1] == 0
            || [sender[0], sender[1]]
                .iter()
                .any(|uid| *uid == target[0] || *uid == target[2])
    }

    /// @description 以一次 caller credential 快照判断 Linux scheduler 修改权限。
    ///
    /// @param target 待修改的 Thread；credentials 由其所属 Process 唯一拥有。
    /// @return 无权限返回 `None`；同 owner 返回 `Some(false)`；effective root 返回 `Some(true)`。
    pub(in crate::task) fn scheduler_privilege_for(
        &self,
        target: &TaskControlBlock,
    ) -> Option<bool> {
        let caller_euid = self.process.credentials.lock().effective_uid;
        let target = target.process.credentials.lock();
        let privileged = caller_euid == ROOT_ID;
        (privileged || caller_euid == target.real_uid || caller_euid == target.effective_uid)
            .then_some(privileged)
    }

    /// @description 原子执行 setuid 或 setgid credential transition。
    pub(crate) fn set_credential_id(&self, uid: bool, value: u32) -> Result<(), ()> {
        let mut credentials = self.process.credentials.lock();
        let previous = if uid {
            credentials.uid(true)
        } else {
            credentials.gid(true)
        };
        let result = if uid {
            credentials.set_uid(value)
        } else {
            credentials.set_gid(value)
        };
        let changed = result.is_ok()
            && previous
                != if uid {
                    credentials.uid(true)
                } else {
                    credentials.gid(true)
                };
        drop(credentials);
        if changed {
            self.clear_parent_death_signal();
        }
        result
    }

    /// @description 原子执行 setresuid 或 setresgid credential transition。
    pub(crate) fn set_credential_res_ids(&self, uid: bool, values: [u32; 3]) -> Result<(), ()> {
        let mut credentials = self.process.credentials.lock();
        let previous = if uid {
            credentials.uid(true)
        } else {
            credentials.gid(true)
        };
        let result = if uid {
            credentials.set_resuids(values)
        } else {
            credentials.set_resgids(values)
        };
        let changed = result.is_ok()
            && previous
                != if uid {
                    credentials.uid(true)
                } else {
                    credentials.gid(true)
                };
        drop(credentials);
        if changed {
            self.clear_parent_death_signal();
        }
        result
    }

    /// @description 复制当前 supplementary group list。
    pub(crate) fn supplementary_groups(&self) -> Result<Vec<u32>, ()> {
        let credentials = self.process.credentials.lock();
        let mut groups = Vec::new();
        groups
            .try_reserve_exact(credentials.groups().len())
            .map_err(|_| ())?;
        groups.extend_from_slice(credentials.groups());
        Ok(groups)
    }

    /// @description 以 effective-root policy 替换 supplementary group list。
    pub(crate) fn set_supplementary_groups(
        &self,
        groups: Vec<u32>,
    ) -> Result<(), CredentialUpdateError> {
        self.process.credentials.lock().set_groups(groups)
    }

    /// @description 原子替换 umask 并返回旧值。
    pub(crate) fn replace_umask(&self, mask: u32) -> u32 {
        self.process.credentials.lock().replace_umask(mask)
    }

    /// @description 将 Process umask 应用于用户提供的 inode mode。
    pub(crate) fn creation_mode(&self, mode: u32) -> u32 {
        self.process.credentials.lock().creation_mode(mode)
    }

    pub(super) fn apply_exec_setid(&self, mode: u32, uid: u32, gid: u32) {
        let mut credentials = self.process.credentials.lock();
        credentials.apply_exec_setid(mode, uid, gid);
        drop(credentials);
        if mode & 0o6000 != 0 {
            self.clear_parent_death_signal();
        }
    }
}

impl Credentials {
    pub(super) fn root() -> Self {
        Self {
            real_uid: ROOT_ID,
            effective_uid: ROOT_ID,
            saved_uid: ROOT_ID,
            real_gid: ROOT_ID,
            effective_gid: ROOT_ID,
            saved_gid: ROOT_ID,
            groups: None,
            umask: DEFAULT_UMASK,
        }
    }

    pub(super) fn access_identity(&self, effective: bool) -> AccessIdentity {
        AccessIdentity::new(
            if effective {
                self.effective_uid
            } else {
                self.real_uid
            },
            if effective {
                self.effective_gid
            } else {
                self.real_gid
            },
            self.groups.clone(),
        )
    }

    pub(super) fn uid(&self, effective: bool) -> u32 {
        if effective {
            self.effective_uid
        } else {
            self.real_uid
        }
    }

    pub(super) fn gid(&self, effective: bool) -> u32 {
        if effective {
            self.effective_gid
        } else {
            self.real_gid
        }
    }

    pub(super) fn resuids(&self) -> [u32; 3] {
        [self.real_uid, self.effective_uid, self.saved_uid]
    }

    pub(super) fn resgids(&self) -> [u32; 3] {
        [self.real_gid, self.effective_gid, self.saved_gid]
    }

    pub(super) fn groups(&self) -> &[u32] {
        self.groups.as_deref().map_or(&[], Vec::as_slice)
    }

    pub(super) fn set_uid(&mut self, uid: u32) -> Result<(), ()> {
        if self.effective_uid == ROOT_ID {
            self.real_uid = uid;
            self.effective_uid = uid;
            self.saved_uid = uid;
            return Ok(());
        }
        if matches!(uid, value if value == self.real_uid || value == self.saved_uid) {
            self.effective_uid = uid;
            Ok(())
        } else {
            Err(())
        }
    }

    pub(super) fn set_gid(&mut self, gid: u32) -> Result<(), ()> {
        if self.effective_uid == ROOT_ID {
            self.real_gid = gid;
            self.effective_gid = gid;
            self.saved_gid = gid;
            return Ok(());
        }
        if matches!(gid, value if value == self.real_gid || value == self.saved_gid) {
            self.effective_gid = gid;
            Ok(())
        } else {
            Err(())
        }
    }

    pub(super) fn set_resuids(&mut self, ids: [u32; 3]) -> Result<(), ()> {
        let permitted = self.effective_uid == ROOT_ID
            || ids.iter().all(|id| {
                *id == UNCHANGED_ID
                    || matches!(*id, value if value == self.real_uid || value == self.effective_uid || value == self.saved_uid)
            });
        if !permitted {
            return Err(());
        }
        if ids[0] != UNCHANGED_ID {
            self.real_uid = ids[0];
        }
        if ids[1] != UNCHANGED_ID {
            self.effective_uid = ids[1];
        }
        if ids[2] != UNCHANGED_ID {
            self.saved_uid = ids[2];
        }
        Ok(())
    }

    pub(super) fn set_resgids(&mut self, ids: [u32; 3]) -> Result<(), ()> {
        let permitted = self.effective_uid == ROOT_ID
            || ids.iter().all(|id| {
                *id == UNCHANGED_ID
                    || matches!(*id, value if value == self.real_gid || value == self.effective_gid || value == self.saved_gid)
            });
        if !permitted {
            return Err(());
        }
        if ids[0] != UNCHANGED_ID {
            self.real_gid = ids[0];
        }
        if ids[1] != UNCHANGED_ID {
            self.effective_gid = ids[1];
        }
        if ids[2] != UNCHANGED_ID {
            self.saved_gid = ids[2];
        }
        Ok(())
    }

    pub(super) fn set_groups(&mut self, groups: Vec<u32>) -> Result<(), CredentialUpdateError> {
        if self.effective_uid != ROOT_ID {
            return Err(CredentialUpdateError::Permission);
        }
        self.groups = if groups.is_empty() {
            None
        } else {
            Some(Arc::try_new(groups).map_err(|_| CredentialUpdateError::OutOfMemory)?)
        };
        Ok(())
    }

    pub(super) fn replace_umask(&mut self, mask: u32) -> u32 {
        let old = self.umask;
        self.umask = mask & 0o777;
        old
    }

    pub(super) fn creation_mode(&self, mode: u32) -> u32 {
        mode & !self.umask & 0o7777
    }

    pub(super) fn apply_exec_setid(&mut self, mode: u32, uid: u32, gid: u32) {
        if mode & 0o4000 != 0 {
            self.effective_uid = uid;
        }
        if mode & 0o2000 != 0 {
            self.effective_gid = gid;
        }
        self.saved_uid = self.effective_uid;
        self.saved_gid = self.effective_gid;
    }
}

/// @description credential replacement 的 permission 与 owner allocation 失败分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CredentialUpdateError {
    Permission,
    OutOfMemory,
}
