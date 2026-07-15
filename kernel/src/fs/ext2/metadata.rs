use super::*;
use crate::fs::permission::OwnerModeState;

impl Ext2InodeDisk {
    pub(super) fn uid(&self) -> u32 {
        self.i_uid as u32 | (u16::from_le_bytes([self.i_osd2[4], self.i_osd2[5]]) as u32) << 16
    }

    pub(super) fn gid(&self) -> u32 {
        self.i_gid as u32 | (u16::from_le_bytes([self.i_osd2[6], self.i_osd2[7]]) as u32) << 16
    }

    pub(super) fn set_uid(&mut self, uid: u32) {
        self.i_uid = uid as u16;
        self.i_osd2[4..6].copy_from_slice(&(uid >> 16).to_le_bytes()[..2]);
    }

    pub(super) fn set_gid(&mut self, gid: u32) {
        self.i_gid = gid as u16;
        self.i_osd2[6..8].copy_from_slice(&(gid >> 16).to_le_bytes()[..2]);
    }
}

impl Ext2Inode {
    pub(super) fn update_times(
        &self,
        atime: Option<u64>,
        mtime: Option<u64>,
    ) -> Result<(), FileSystemError> {
        if atime.is_none() && mtime.is_none() {
            return Ok(());
        }
        let atime = atime
            .map(u32::try_from)
            .transpose()
            .map_err(|_| FileSystemError::InvalidOperation)?;
        let mtime = mtime
            .map(u32::try_from)
            .transpose()
            .map_err(|_| FileSystemError::InvalidOperation)?;
        let mut mutation = self.fs.begin_mutation()?;
        let mut inode = mutation.inode(self)?;
        if let Some(value) = atime {
            inode.i_atime = value;
        }
        if let Some(value) = mtime {
            inode.i_mtime = value;
        }
        inode.i_ctime = Self::now();
        self.fs.write_inode_disk(self.inode_num, &inode)?;
        drop(inode);
        mutation.commit()
    }

    pub(super) fn update_owner_mode(&self, change: OwnerModeChange) -> Result<(), FileSystemError> {
        // mutation lock 先冻结 live owner/mode；拒绝路径不得为全 inode rollback snapshot 分配。
        let (mut mutation, update) = MutationGuard::begin_after(&self.fs, || {
            let disk = self.disk.lock();
            change.authorize(OwnerModeState::new(
                Self::kind_from_mode(disk.i_mode),
                disk.i_mode,
                disk.uid(),
                disk.gid(),
            ))
        })?;
        let mut disk = mutation.inode(self)?;
        disk.i_mode = update.mode();
        disk.set_uid(update.uid());
        disk.set_gid(update.gid());
        disk.i_ctime = Self::now();
        self.fs.write_inode_disk(self.inode_num, &disk)?;
        drop(disk);
        mutation.commit()
    }
}
