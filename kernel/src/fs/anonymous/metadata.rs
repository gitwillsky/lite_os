//! Pseudo inode 的单一 owner/mode/time tuple；不拥有实际 endpoint 或 content pages。
use super::PseudoFileSystem;
use crate::fs::inode_times::TimeOwner;
use crate::fs::{
    CreateMetadata, FileSystemError, InodeMetadata, InodeType, OwnerModeChange,
    permission::OwnerModeState,
};
use spin::Mutex;

struct MetadataState {
    owner: OwnerModeState,
    times: TimeOwner,
}

/// pseudo inode 的唯一 owner/mode/time tuple；content size/seals 仍由实际 MemoryFile 拥有。
///
/// 此锁持 owner scalar 与唯一时间 owner 的引用；guard 内不分配、不消费 credential groups，
/// 亦不 pin 任何实际 endpoint。Memory 时间引用在整个 inode lifetime 内保持不变。
pub(in crate::fs) struct Metadata {
    filesystem: PseudoFileSystem,
    inode: u64,
    kind: InodeType,
    links: u32,
    state: Mutex<MetadataState>,
}
impl Metadata {
    pub(in crate::fs) fn new(
        filesystem: PseudoFileSystem,
        inode: u64,
        kind: InodeType,
        creator: CreateMetadata,
        links: u32,
        times: TimeOwner,
    ) -> Self {
        Self {
            filesystem,
            inode,
            kind,
            links,
            state: Mutex::new(MetadataState {
                owner: OwnerModeState::new(kind, creator.mode as u16, creator.uid, creator.gid),
                times,
            }),
        }
    }
    pub(in crate::fs) fn snapshot(&self, size: u64, blocks: u64) -> InodeMetadata {
        let state = self.state.lock();
        let times = state.times.snapshot();
        InodeMetadata {
            filesystem: self.filesystem.id() as u64,
            inode: self.inode,
            kind: self.kind,
            mode: u32::from(state.owner.mode()),
            uid: state.owner.uid(),
            gid: state.owner.gid(),
            links: self.links,
            size,
            blocks,
            block_size: 4096,
            atime: times.atime,
            mtime: times.mtime,
            ctime: times.ctime,
            fine_times: Default::default(),
            device: None,
        }
    }
    pub(in crate::fs) fn change(&self, change: &OwnerModeChange) -> Result<(), FileSystemError> {
        let mut state = self.state.lock();
        let updated = change.authorize(OwnerModeState::new(
            self.kind,
            state.owner.mode(),
            state.owner.uid(),
            state.owner.gid(),
        ))?;
        state.owner = updated;
        state.times.change_at(now_seconds());
        Ok(())
    }

    pub(in crate::fs) fn set_times(&self, atime: Option<u64>, mtime: Option<u64>) {
        let now = now_seconds();
        self.state.lock().times.set_at(now, atime, mtime);
    }
}
fn now_seconds() -> u64 {
    crate::timer::get_realtime_ns() / 1_000_000_000
}
