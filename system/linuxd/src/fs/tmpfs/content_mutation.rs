//! tmpfs canonical mode owner 的 content/owner mutation；write sequence 先于 metadata State。
use super::*;
impl TmpInode {
    pub(super) fn file(&self) -> Result<&Arc<MemoryFile>, FileSystemError> {
        match &self.body {
            Body::File(file) => Ok(file),
            Body::Directory => Err(FileSystemError::IsDirectory),
            Body::Symlink(_) | Body::Special(..) => Err(FileSystemError::InvalidOperation),
        }
    }

    pub(super) fn remove_write_privileges(
        &self,
        identity: Option<&crate::fs::AccessIdentity>,
    ) -> Result<(), FileSystemError> {
        let Some(identity) = identity else {
            return Ok(());
        };
        let mut state = self.state()?;
        self.clear_write_privileges(&mut state, identity);
        Ok(())
    }

    /// 成功 allocation 后只能阻塞，不能因 metadata waiter 分配失败保留set-ID。
    pub(super) fn remove_write_privileges_prepared(
        &self,
        identity: Option<&crate::fs::AccessIdentity>,
        wait: &mut crate::execution::sync::TaskMutexWaitPreparation,
    ) {
        if let Some(identity) = identity {
            let mut state = self.state.lock_prepared(wait);
            self.clear_write_privileges(&mut state, identity);
        }
    }

    pub(super) fn clear_write_privileges(
        &self,
        state: &mut State,
        identity: &crate::fs::AccessIdentity,
    ) {
        let kill = identity.write_kill_mask(self.kind(), state.mode, state.gid);
        // FSETID只豁免set-ID位；任何实际非零capability属性仍必须去除。
        // 表只持byte buffers，摘除/释放无外部回调或重新取锁，不要求准备阶段再分配。
        let capability_removed = state.xattrs.kill_capability().is_some();
        if kill != 0 || capability_removed {
            state.mode &= !kill;
            state.times.change_at(now_seconds());
        }
    }

    pub(super) fn update_owner_mode(&self, change: OwnerModeChange) -> Result<(), FileSystemError> {
        let _sequence = match &self.body {
            Body::File(file) => Some(file.begin_write()?),
            _ => None,
        };
        let mut state = self.state()?;
        let kill_capability = change.kills_privileges(self.kind());
        let updated = change.authorize(OwnerModeState::new(
            self.kind(),
            state.mode,
            state.uid,
            state.gid,
        ))?;
        state.mode = updated.mode();
        state.uid = updated.uid();
        state.gid = updated.gid();
        let retired = if kill_capability {
            state.xattrs.kill_capability()
        } else {
            None
        };
        state.times.change_at(now_seconds());
        drop(state);
        drop(retired);
        Ok(())
    }
}
