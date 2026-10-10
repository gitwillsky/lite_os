//! proc registration 拥有 mode/UID/GID；live inode 只另拥有自己的时间，不复制注册属性。
use super::{FileSystemError, InodeMetadata, ProcNode};
use crate::fs::{OwnerModeChange, inode_times::TimeOwner, permission::OwnerModeState};
use alloc::{sync::Arc, vec::Vec};
use spin::{Mutex, Once};

type Owner = Arc<Mutex<OwnerModeState>>;
// OWNER: generic proc_dir_entry 属性随注册而存在，跨挂载/最后 inode close 不丢 chmod/chown。
// Once 只安装完整数组；全部 allocation 与失败 candidate Drop 在其锁外完成。
static REGISTERED_ENTRIES: Once<[Owner; ProcNode::GENERIC_ENTRY_COUNT]> = Once::new();

/// 挂载前装配固定 generic proc entries；PID/sysctl 属性不登记在全局 entry 表。
pub(super) fn register_entries() -> Result<(), FileSystemError> {
    if REGISTERED_ENTRIES.get().is_some() {
        return Ok(());
    }
    let mut entries = Vec::new();
    entries
        .try_reserve_exact(ProcNode::GENERIC_ENTRY_COUNT)
        .map_err(|_| FileSystemError::OutOfMemory)?;
    for node in ProcNode::generic_entries() {
        entries.push(
            Arc::try_new(Mutex::new(OwnerModeState::new(
                node.kind(),
                node.metadata_mode() as u16,
                0,
                0,
            )))
            .map_err(|_| FileSystemError::OutOfMemory)?,
        );
    }
    let entries: [Owner; ProcNode::GENERIC_ENTRY_COUNT] = match entries.try_into() {
        Ok(entries) => entries,
        Err(_) => unreachable!("fixed proc registration count"),
    };
    REGISTERED_ENTRIES.call_once(|| entries);
    Ok(())
}
/// 唯一 inode attribute owner：注册 entry 的 canonical 属性或 PID/sysctl 的私有属性，加 inode 时间。
/// 多锁只按 owner→times 获取；短锁内不取得 Task、snapshot 或 namespace 锁，不分配/析构 group。
pub(super) struct ProcAttributes {
    owner: Owner,
    times: Mutex<TimeOwner>,
}
impl ProcAttributes {
    pub(super) fn new(node: ProcNode, metadata: InodeMetadata) -> Result<Self, FileSystemError> {
        let owner = if let Some(index) = node.generic_index() {
            REGISTERED_ENTRIES
                .get()
                .expect("proc registration precedes inode creation")[index]
                .clone()
        } else {
            Arc::try_new(Mutex::new(OwnerModeState::new(
                metadata.kind,
                metadata.mode as u16,
                metadata.uid,
                metadata.gid,
            )))
            .map_err(|_| FileSystemError::OutOfMemory)?
        };
        Ok(Self {
            owner,
            times: Mutex::new(TimeOwner::new()),
        })
    }
    pub(super) fn snapshot(&self, mut metadata: InodeMetadata) -> InodeMetadata {
        let owner = self.owner.lock();
        metadata.mode = u32::from(owner.mode());
        metadata.uid = owner.uid();
        metadata.gid = owner.gid();
        let times = self.times.lock().snapshot();
        metadata.atime = times.atime;
        metadata.mtime = times.mtime;
        metadata.ctime = times.ctime;
        metadata
    }
    /// pid_revalidate 的 owner refresh 不改变时间；getattr 另投影 live Task 的可见 owner。
    pub(super) fn refresh_owner(&self, uid: u32, gid: u32) {
        let mut owner = self.owner.lock();
        *owner = OwnerModeState::new(owner.kind(), owner.mode() & !0o6000, uid, gid);
    }
    pub(super) fn change(
        &self,
        node: ProcNode,
        change: &OwnerModeChange,
    ) -> Result<(), FileSystemError> {
        // proc_nochmod_setattr 仅拒绝 PID mode；proc_sys_setattr 拒绝 mode/UID/GID。
        if node.owner_tid().is_some() && change.changes_mode()
            || matches!(
                node,
                ProcNode::UserResourceLimit(_)
                    | ProcNode::UserSysctlDir
                    | ProcNode::SysDir
                    | ProcNode::NetSysctlDir
                    | ProcNode::CoreNetSysctlDir
                    | ProcNode::CoreBufferLimit(_)
                    | ProcNode::OptmemLimit
                    | ProcNode::FsSysctlDir
                    | ProcNode::InotifySysctlDir
                    | ProcNode::InotifyLimit(_)
                    | ProcNode::MountLimit
                    | ProcNode::KernelSysctlDir
                    | ProcNode::PidLimit
                    | ProcNode::IpcLimit(_)
            )
        {
            return Err(FileSystemError::PermissionDenied);
        }
        let now = crate::timer::get_realtime_ns() / 1_000_000_000;
        let mut owner = self.owner.lock();
        *owner = change.authorize(OwnerModeState::new(
            owner.kind(),
            owner.mode(),
            owner.uid(),
            owner.gid(),
        ))?;
        self.times.lock().change_at(now);
        Ok(())
    }
    pub(super) fn truncate(&self) {
        let now = crate::timer::get_realtime_ns() / 1_000_000_000;
        self.times.lock().touch_at(now);
    }
    pub(super) fn touch_access(&self, now: u64, relatime: bool) {
        self.times.lock().access_at(now, relatime);
    }
    pub(super) fn set_times(&self, atime: Option<u64>, mtime: Option<u64>) {
        let now = crate::timer::get_realtime_ns() / 1_000_000_000;
        self.times.lock().set_at(now, atime, mtime);
    }
}
