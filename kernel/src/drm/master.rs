use super::{DrmError, DrmFile};

impl DrmFile {
    /// @description 判断该 OFD 是否拥有 primary-node KMS master。
    /// @return device master identity 与本 OFD 相同时返回 true。
    pub(crate) fn is_master(&self) -> bool {
        self.device.state.lock().master == Some(self.file_identity)
    }

    /// @description 按 Linux primary-node 语义取得 KMS master。
    /// @param privileged caller 是否具有 CAP_SYS_ADMIN 等价权限。
    /// @return 已是 master 或成功取得 ownership 返回 unit。
    /// @errors 已有其他 master 返回 Busy；既非 privileged 也未曾为 master 返回 Permission。
    pub(crate) fn set_master(&self, privileged: bool) -> Result<(), DrmError> {
        if !privileged && !self.state.lock().was_master {
            return Err(DrmError::Permission);
        }
        let mut device = self.device.state.lock();
        match device.master {
            Some(master) if master == self.file_identity => return Ok(()),
            Some(_) => return Err(DrmError::Busy),
            None => device.master = Some(self.file_identity),
        }
        drop(device);
        self.state.lock().was_master = true;
        Ok(())
    }

    /// @description 按 Linux primary-node 语义释放当前 KMS master。
    /// @param privileged caller 是否具有 CAP_SYS_ADMIN 等价权限。
    /// @return 当前 OFD 的 master ownership 已释放返回 unit。
    /// @errors caller 无权操作返回 Permission；当前 OFD 不是 master 返回 Invalid。
    pub(crate) fn drop_master(&self, privileged: bool) -> Result<(), DrmError> {
        if !privileged && !self.state.lock().was_master {
            return Err(DrmError::Permission);
        }
        let mut device = self.device.state.lock();
        if device.master != Some(self.file_identity) {
            return Err(DrmError::Invalid);
        }
        device.master = None;
        Ok(())
    }
}
