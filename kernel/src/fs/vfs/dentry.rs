//! Filesystem directory entry 的唯一关系 owner；mount view 与 OFD 不复制 parent/name/deleted。
use super::dentry_index::DentryKey;
use super::{FileSystemError, Inode};
use alloc::{boxed::Box, sync::Arc, vec::Vec};
use spin::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct FileName {
    length: u8,
    bytes: [u8; 255],
}
impl FileName {
    pub(super) fn new(name: &[u8]) -> Result<Self, FileSystemError> {
        if name.len() > 255 {
            return Err(FileSystemError::NameTooLong);
        }
        let mut value = Self {
            length: name.len() as u8,
            bytes: [0; 255],
        };
        value.bytes[..name.len()].copy_from_slice(name);
        Ok(value)
    }
    pub(super) fn bytes(&self) -> &[u8] {
        &self.bytes[..usize::from(self.length)]
    }
}
struct Location {
    parent: Option<Arc<Dentry>>,
    name: FileName,
    deleted: bool,
    registration: Option<DentryKey>,
}
enum Place {
    Named(Box<Mutex<Location>>),
    // 不可变诊断标签不作为 pathname 展开，也不登记 namespace index。
    Anonymous(Vec<u8>),
}

/// Canonical live dentry，属于 filesystem 而非某一个 mount namespace。
/// parent 仅指 filesystem 内真实父 dentry；跨 mount 的路径关系属于 MountView attachment。
pub(in crate::fs) struct Dentry {
    inode: Arc<dyn Inode>,
    place: Place,
}
impl Dentry {
    pub(super) fn named(
        inode: Arc<dyn Inode>,
        parent: Option<Arc<Self>>,
        name: &[u8],
    ) -> Result<Arc<Self>, FileSystemError> {
        let inode = super::super::block_node::wrap_block_node(inode)?;
        let location = Box::try_new(Mutex::new(Location {
            parent,
            name: FileName::new(name)?,
            deleted: false,
            registration: None,
        }))
        .map_err(|_| FileSystemError::OutOfMemory)?;
        Arc::try_new(Self {
            inode,
            place: Place::Named(location),
        })
        .map_err(|_| FileSystemError::OutOfMemory)
    }
    pub(in crate::fs) fn anonymous(
        inode: Arc<dyn Inode>,
        label: Vec<u8>,
    ) -> Result<Arc<Self>, FileSystemError> {
        Arc::try_new(Self {
            inode,
            place: Place::Anonymous(label),
        })
        .map_err(|_| FileSystemError::OutOfMemory)
    }
    pub(super) fn inode(&self) -> Arc<dyn Inode> {
        self.inode.clone()
    }
    pub(super) fn inode_identity(&self) -> Result<(usize, u64), FileSystemError> {
        Ok((self.inode.filesystem_id(), self.inode.identity()?.1))
    }
    fn location(&self) -> Option<&Mutex<Location>> {
        match &self.place {
            Place::Named(location) => Some(location),
            Place::Anonymous(_) => None,
        }
    }
    pub(super) fn snapshot(&self) -> Option<(Option<Arc<Self>>, FileName, bool)> {
        let location = self.location()?.lock();
        Some((location.parent.clone(), location.name, location.deleted))
    }
    pub(super) fn diagnostic_label(&self) -> Option<&[u8]> {
        match &self.place {
            Place::Anonymous(label) => Some(label),
            Place::Named(_) => None,
        }
    }
    pub(super) fn is_unlinked(&self) -> bool {
        self.location()
            .is_some_and(|location| location.lock().deleted)
    }
    pub(super) fn index_key(&self) -> Result<Option<DentryKey>, FileSystemError> {
        let Some(location) = self.location() else {
            return Ok(None);
        };
        let (parent, name) = {
            let location = location.lock();
            (location.parent.clone(), location.name)
        };
        Ok(Some(DentryKey {
            parent: parent
                .as_ref()
                .map(|parent| parent.inode_identity())
                .transpose()?,
            name,
            inode: self.inode_identity()?,
        }))
    }
    pub(super) fn publish_registration(&self, key: DentryKey) {
        let mut location = self.location().expect("named dentry registration").lock();
        assert!(
            location.registration.is_none() && !location.deleted,
            "dentry admitted twice"
        );
        location.registration = Some(key);
    }
    pub(super) fn mark_deleted(&self, key: DentryKey) {
        let mut location = self.location().expect("named dentry unlink").lock();
        assert_eq!(location.registration.take(), Some(key));
        location.deleted = true;
    }
    pub(super) fn move_to(
        &self,
        key: DentryKey,
        parent: Arc<Self>,
        name: FileName,
        new_key: DentryKey,
    ) -> Arc<Self> {
        let mut location = self.location().expect("named dentry rename").lock();
        assert_eq!(location.registration, Some(key));
        assert!(!location.deleted, "rename admitted deleted dentry");
        let retired = location
            .parent
            .replace(parent)
            .expect("rename filesystem root");
        location.name = name;
        location.registration = Some(new_key);
        retired
    }
    fn take_parent(&mut self) -> Option<Arc<Self>> {
        match &mut self.place {
            Place::Named(location) => location.get_mut().parent.take(),
            Place::Anonymous(_) => None,
        }
    }
}
impl Drop for Dentry {
    fn drop(&mut self) {
        let Place::Named(location) = &mut self.place else {
            return;
        };
        let location = location.get_mut();
        let deleted = location.deleted;
        if let Some(key) = location.registration.take() {
            super::vfs().dentries.unregister(key, self);
        }
        if deleted {
            super::super::page_cache::evict_if_unlinked(&self.inode);
            // Linux dentry_unlink_inode：最后 actual path pin 消失才完成 DELETE_SELF/IGNORED。
            // metadata/event allocation/user wake 均不保留 location/index guard，匿名 entry 不进入。
            crate::fs::notify_dentry_reclaimed(self.inode.as_ref());
        }
        // 深层 cwd/FD 可独占整条 parent chain，递归释放会耗尽 kernel stack。
        // 只拆唯一父 owner；遇到仍共享的 live owner 即停止，不改变其 identity。
        let mut parent = self.take_parent();
        while let Some(entry) = parent {
            match Arc::try_unwrap(entry) {
                Ok(mut entry) => {
                    parent = entry.take_parent();
                    drop(entry);
                }
                Err(shared) => {
                    drop(shared);
                    break;
                }
            }
        }
    }
}
