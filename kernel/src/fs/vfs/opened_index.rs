use alloc::sync::{Arc, Weak};
use spin::Mutex;

use super::{FileSystemError, OpenedFile, opened::FileName};
use crate::fallible_tree::FallibleMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct OpenedPathKey {
    pub(super) parent: (usize, u64),
    pub(super) name: FileName,
    pub(super) inode: (usize, u64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct OpenedIndexKey {
    path: OpenedPathKey,
    registration: usize,
}

/// @description live OpenedFile 的唯一 exact lifecycle/path index。
pub(super) struct OpenedIndex {
    // key 以 namespace path identity 为前缀、Arc allocation identity 为末段；同一
    // directory entry 的重复 lookup 各有独立 membership，rename/unlink 只访问前缀范围。
    // Weak 只用于 exact node 的 lifetime pin；upgrade 成功的临时 Arc 排除 final Drop，
    // upgrade 失败则不解引用对象并等待 Drop 精确删除该 node。
    entries: Mutex<FallibleMap<OpenedIndexKey, Weak<OpenedFile>>>,
}

impl OpenedIndex {
    pub(super) const fn new() -> Self {
        Self {
            entries: Mutex::new(FallibleMap::new()),
        }
    }

    /// @description 预分配并发布一个 exact opened membership。
    pub(super) fn register(
        &self,
        opened: Arc<OpenedFile>,
    ) -> Result<Arc<OpenedFile>, FileSystemError> {
        let Some(path) = opened.index_path()? else {
            return Ok(opened);
        };
        let key = OpenedIndexKey {
            path,
            registration: Arc::as_ptr(&opened) as usize,
        };
        let prepared = FallibleMap::try_prepare(key, Arc::downgrade(&opened))
            .map_err(|_| FileSystemError::OutOfMemory)?;
        let mut entries = self.entries.lock();
        entries.commit_vacant(prepared);
        opened.publish_registration(key);
        Ok(opened)
    }

    /// @description OpenedFile final Drop 精确撤销唯一 membership。
    pub(super) fn unregister(&self, key: OpenedIndexKey) {
        assert!(
            self.entries.lock().remove(&key).is_some(),
            "opened lifecycle membership disappeared before final drop"
        );
    }

    /// @description 只标记精确 namespace entry 前缀下的 live opened instances。
    pub(super) fn mark_unlinked(&self, parent: (usize, u64), name: &[u8], inode: (usize, u64)) {
        let path = OpenedPathKey {
            parent,
            name: FileName::new(name).expect("VFS accepted an overlong component"),
            inode,
        };
        let lower = OpenedIndexKey {
            path,
            registration: 0,
        };
        let mut cursor = None;
        loop {
            let next = {
                let entries = self.entries.lock();
                match cursor {
                    Some(cursor) => entries.successor(&cursor),
                    None => entries.ceiling(&lower),
                }
                .map(|(key, opened)| (*key, opened.clone()))
            };
            let Some((key, opened)) = next else {
                break;
            };
            if key.path != path {
                break;
            }
            cursor = Some(key);
            if let Some(opened) = opened.upgrade() {
                opened.mark_deleted(key);
            }
        }
    }

    /// @description 回收原 membership 节点并无分配地移动精确前缀范围。
    pub(super) fn move_entries(
        &self,
        old_parent: (usize, u64),
        old_name: &[u8],
        inode: (usize, u64),
        new_parent: Arc<OpenedFile>,
        new_parent_identity: (usize, u64),
        new_name: &[u8],
    ) {
        let old_path = OpenedPathKey {
            parent: old_parent,
            name: FileName::new(old_name).expect("VFS accepted an overlong component"),
            inode,
        };
        let new_name = FileName::new(new_name).expect("VFS accepted an overlong component");
        let new_path = OpenedPathKey {
            parent: new_parent_identity,
            name: new_name,
            inode,
        };
        let lower = OpenedIndexKey {
            path: old_path,
            registration: 0,
        };
        let mut cursor = None;
        loop {
            let next = {
                let entries = self.entries.lock();
                match cursor {
                    Some(cursor) => entries.successor(&cursor),
                    None => entries.ceiling(&lower),
                }
                .map(|(key, opened)| (*key, opened.clone()))
            };
            let Some((key, opened)) = next else {
                break;
            };
            if key.path != old_path {
                break;
            }
            cursor = Some(key);
            let Some(opened) = opened.upgrade() else {
                continue;
            };
            let new_key = OpenedIndexKey {
                path: new_path,
                registration: key.registration,
            };
            if old_path == new_path {
                let retired_parent = opened.move_to(key, new_parent.clone(), new_name, new_key);
                drop(retired_parent);
                continue;
            }
            let mut entries = self.entries.lock();
            let mut node = entries
                .take_entry(&key)
                .expect("selected opened membership disappeared");
            let retired_parent = opened.move_to(key, new_parent.clone(), new_name, new_key);
            if retired_parent.is_some() {
                node.set_key(new_key);
            }
            entries.commit_vacant(node);
            // Arc consequence 必须在 index lock 外析构；旧 parent 或 upgrade pin 可能是
            // 最后一个 strong ref，其 Drop 会精确 unregister 并重取本锁。
            drop(entries);
            drop(retired_parent);
            drop(opened);
        }
    }
}
