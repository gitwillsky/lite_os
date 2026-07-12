use alloc::vec::Vec;

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
    groups: Vec<u32>,
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

    /// @description 原子执行 setuid 或 setgid credential transition。
    pub(crate) fn set_credential_id(&self, uid: bool, value: u32) -> Result<(), ()> {
        let mut credentials = self.process.credentials.lock();
        if uid {
            credentials.set_uid(value)
        } else {
            credentials.set_gid(value)
        }
    }

    /// @description 原子执行 setresuid 或 setresgid credential transition。
    pub(crate) fn set_credential_res_ids(&self, uid: bool, values: [u32; 3]) -> Result<(), ()> {
        let mut credentials = self.process.credentials.lock();
        if uid {
            credentials.set_resuids(values)
        } else {
            credentials.set_resgids(values)
        }
    }

    /// @description 复制当前 supplementary group list。
    pub(crate) fn supplementary_groups(&self) -> Vec<u32> {
        self.process.credentials.lock().groups().to_vec()
    }

    /// @description 以 effective-root policy 替换 supplementary group list。
    pub(crate) fn set_supplementary_groups(&self, groups: Vec<u32>) -> Result<(), ()> {
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
        self.process
            .credentials
            .lock()
            .apply_exec_setid(mode, uid, gid);
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
            groups: Vec::new(),
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
        &self.groups
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

    pub(super) fn set_groups(&mut self, groups: Vec<u32>) -> Result<(), ()> {
        if self.effective_uid != ROOT_ID {
            return Err(());
        }
        self.groups = groups;
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
