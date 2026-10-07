use super::inode::Timestamp;
use super::*;
use crate::fs::permission::OwnerModeState;

impl Ext4InodeDisk {
    pub(super) fn uid(&self) -> u32 {
        u32::from(self.i_uid) | u32::from(self.i_uid_high) << 16
    }

    pub(super) fn gid(&self) -> u32 {
        u32::from(self.i_gid) | u32::from(self.i_gid_high) << 16
    }

    pub(super) fn set_uid(&mut self, uid: u32) {
        self.i_uid = uid as u16;
        self.i_uid_high = (uid >> 16) as u16;
    }

    pub(super) fn set_gid(&mut self, gid: u32) {
        self.i_gid = gid as u16;
        self.i_gid_high = (gid >> 16) as u16;
    }

    pub(super) fn atime(&self) -> Timestamp {
        Timestamp::decode(self.i_atime, self.i_atime_extra)
    }

    pub(super) fn mtime(&self) -> Timestamp {
        Timestamp::decode(self.i_mtime, self.i_mtime_extra)
    }

    pub(super) fn ctime(&self) -> Timestamp {
        Timestamp::decode(self.i_ctime, self.i_ctime_extra)
    }

    pub(super) fn set_atime(&mut self, time: Timestamp) {
        (self.i_atime, self.i_atime_extra) = time.encode();
    }

    pub(super) fn set_mtime(&mut self, time: Timestamp) {
        (self.i_mtime, self.i_mtime_extra) = time.encode();
    }

    pub(super) fn set_ctime(&mut self, time: Timestamp) {
        (self.i_ctime, self.i_ctime_extra) = time.encode();
    }

    pub(super) fn set_crtime(&mut self, time: Timestamp) {
        (self.i_crtime, self.i_crtime_extra) = time.encode();
    }
}

impl Ext4Inode {
    pub(super) fn update_times(
        &self,
        atime: Option<u64>,
        mtime: Option<u64>,
    ) -> Result<(), FileSystemError> {
        if atime.is_none() && mtime.is_none() {
            return Ok(());
        }
        let atime = atime.map(Timestamp::from_seconds).transpose()?;
        let mtime = mtime.map(Timestamp::from_seconds).transpose()?;
        let mut mutation = self.fs.begin_mutation()?;
        let mut inode = mutation.inode(self)?;
        if let Some(value) = atime {
            inode.set_atime(value);
        }
        if let Some(value) = mtime {
            inode.set_mtime(value);
        }
        inode.set_ctime(Timestamp::now());
        self.fs.write_inode_disk(self.inode_num, &inode)?;
        drop(inode);
        mutation.commit()
    }

    pub(super) fn update_owner_mode(&self, change: OwnerModeChange) -> Result<(), FileSystemError> {
        // mutation lock 先冻结 live owner/mode；拒绝路径不得为全 inode rollback snapshot 分配。
        let (mut mutation, update) = MutationGuard::begin_after(&self.fs, || {
            let disk = self.disk.lock();
            change.authorize(OwnerModeState::new(
                inode_kind::from_mode(disk.i_mode),
                disk.i_mode,
                disk.uid(),
                disk.gid(),
            ))
        })?;
        let mut disk = mutation.inode(self)?;
        disk.i_mode = update.mode();
        disk.set_uid(update.uid());
        disk.set_gid(update.gid());
        disk.set_ctime(Timestamp::now());
        self.fs.write_inode_disk(self.inode_num, &disk)?;
        drop(disk);
        mutation.commit()
    }
}
