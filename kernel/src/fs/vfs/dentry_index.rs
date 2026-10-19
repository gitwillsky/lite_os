//! Canonical dentry lifecycle/path admission；namespace clones share this filesystem owner。
use super::{
    FileSystemError,
    dentry::{Dentry, FileName},
};
use crate::fallible_tree::FallibleMap;
use alloc::sync::{Arc, Weak};
use spin::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct DentryKey {
    pub(super) parent: Option<(usize, u64)>,
    pub(super) name: FileName,
    pub(super) inode: (usize, u64),
}
/// 单一 exact Weak index；lookup/rename/unlink O(log n)，无 alias 扫描或退休 entry sweep。
/// Node/candidate 锁外准备，摘除节点与所有 final Arc/Weak consequence 锁外释放。
pub(super) struct DentryIndex(Mutex<FallibleMap<DentryKey, Weak<Dentry>>>);
impl DentryIndex {
    pub(super) const fn new() -> Self {
        Self(Mutex::new(FallibleMap::new()))
    }
    pub(super) fn admit(&self, candidate: Arc<Dentry>) -> Result<Arc<Dentry>, FileSystemError> {
        let Some(key) = candidate.index_key()? else {
            return Ok(candidate);
        };
        let observed = { self.0.lock().get(&key).and_then(Weak::upgrade) };
        if let Some(live) = observed {
            return Ok(live);
        }
        let prepared = FallibleMap::try_prepare(key, Arc::downgrade(&candidate))
            .map_err(|_| FileSystemError::OutOfMemory)?;
        let (selected, retired, unused) = {
            let mut entries = self.0.lock();
            if let Some(live) = entries.get(&key).and_then(Weak::upgrade) {
                (live, None, Some(prepared))
            } else {
                let retired = entries.take_entry(&key);
                entries.commit_vacant(prepared);
                candidate.publish_registration(key);
                (candidate.clone(), retired, None)
            }
        };
        drop(retired);
        drop(unused);
        drop(candidate);
        Ok(selected)
    }
    pub(super) fn unregister(&self, key: DentryKey, owner: &Dentry) {
        let retired = {
            let mut entries = self.0.lock();
            if entries
                .get(&key)
                .is_some_and(|entry| core::ptr::eq(entry.as_ptr(), owner))
            {
                entries.take_entry(&key)
            } else {
                None
            }
        };
        drop(retired);
    }
    pub(super) fn mark_unlinked(&self, parent: (usize, u64), name: &[u8], inode: (usize, u64)) {
        let key = DentryKey {
            parent: Some(parent),
            name: FileName::new(name).expect("accepted component"),
            inode,
        };
        let (entry, retired) = {
            let mut entries = self.0.lock();
            let entry = entries.get(&key).and_then(Weak::upgrade);
            let retired = entries.take_entry(&key);
            if let Some(entry) = &entry {
                entry.mark_deleted(key);
            }
            (entry, retired)
        };
        drop(retired);
        drop(entry);
    }
    pub(super) fn move_entry(
        &self,
        old_parent: (usize, u64),
        old_name: &[u8],
        inode: (usize, u64),
        new_parent: Arc<Dentry>,
        new_parent_identity: (usize, u64),
        new_name: &[u8],
    ) {
        let old = DentryKey {
            parent: Some(old_parent),
            name: FileName::new(old_name).expect("accepted source component"),
            inode,
        };
        let name = FileName::new(new_name).expect("accepted target component");
        let next = DentryKey {
            parent: Some(new_parent_identity),
            name,
            inode,
        };
        if old == next {
            return;
        }
        let (entry, retired_parent) = {
            let mut entries = self.0.lock();
            let Some(entry) = entries.get(&old).and_then(Weak::upgrade) else {
                return;
            };
            let mut node = entries
                .take_entry(&old)
                .expect("pinned dentry registration");
            let retired = entry.move_to(old, new_parent, name, next);
            node.set_key(next);
            entries.commit_vacant(node);
            (entry, retired)
        };
        drop(retired_parent);
        drop(entry);
    }
}
