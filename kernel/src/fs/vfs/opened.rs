use alloc::{sync::Arc, vec::Vec};
use spin::Mutex;

use super::{FileSystemError, Inode, VirtualFileSystem};

struct OpenedLocation {
    parent: Option<Arc<OpenedFile>>,
    name: FileName,
    deleted: bool,
}

#[derive(Clone, Copy)]
struct FileName {
    length: u8,
    bytes: [u8; 255],
}

impl FileName {
    fn new(name: &[u8]) -> Result<Self, FileSystemError> {
        if name.len() > 255 {
            return Err(FileSystemError::InvalidPath);
        }
        let mut value = Self {
            length: name.len() as u8,
            bytes: [0; 255],
        };
        value.bytes[..name.len()].copy_from_slice(name);
        Ok(value)
    }

    fn bytes(&self) -> &[u8] {
        &self.bytes[..usize::from(self.length)]
    }
}

/// @description VFS namespace 中一次 pathname lookup 得到的稳定 opened-entry identity。
pub(crate) struct OpenedFile {
    inode: Arc<dyn Inode>,
    // OWNER: VFS 唯一更新打开目录项的 parent/name/deleted 关系；若 OFD 自行缓存路径，
    // rename、hardlink 与 unlink 后 `/proc/<pid>/fd` 会出现彼此矛盾的路径。
    location: Mutex<OpenedLocation>,
}

impl OpenedFile {
    pub(super) fn root(inode: Arc<dyn Inode>) -> Result<Arc<Self>, FileSystemError> {
        Arc::try_new(Self {
            inode,
            location: Mutex::new(OpenedLocation {
                parent: None,
                name: FileName::new(&[])?,
                deleted: false,
            }),
        })
        .map_err(|_| FileSystemError::OutOfMemory)
    }

    pub(super) fn child(
        inode: Arc<dyn Inode>,
        parent: Arc<OpenedFile>,
        name: &[u8],
    ) -> Result<Arc<Self>, FileSystemError> {
        Arc::try_new(Self {
            inode,
            location: Mutex::new(OpenedLocation {
                parent: Some(parent),
                name: FileName::new(name)?,
                deleted: false,
            }),
        })
        .map_err(|_| FileSystemError::OutOfMemory)
    }

    pub(crate) fn inode(&self) -> Arc<dyn Inode> {
        self.inode.clone()
    }

    pub(super) fn parent(&self) -> Option<Arc<OpenedFile>> {
        self.location.lock().parent.clone()
    }

    pub(super) fn location_name(&self) -> Result<Vec<u8>, FileSystemError> {
        let location = self.location.lock();
        let mut name = Vec::new();
        name.try_reserve_exact(location.name.bytes().len())
            .map_err(|_| FileSystemError::OutOfMemory)?;
        name.extend_from_slice(location.name.bytes());
        Ok(name)
    }

    pub(super) fn matches(
        &self,
        parent: &Arc<OpenedFile>,
        name: &[u8],
        inode_identity: (usize, u64),
    ) -> bool {
        let location = self.location.lock();
        !location.deleted
            && location.name.bytes() == name
            && location
                .parent
                .as_ref()
                .is_some_and(|candidate| candidate.same_inode(parent))
            && self.inode_identity().ok() == Some(inode_identity)
    }

    pub(super) fn mark_deleted(&self) {
        self.location.lock().deleted = true;
    }

    pub(super) fn move_to(&self, parent: Arc<OpenedFile>, name: &[u8]) {
        let mut location = self.location.lock();
        location.parent = Some(parent);
        location.name = FileName::new(name).expect("VFS accepted an overlong component");
    }

    pub(super) fn same_inode(&self, other: &Arc<OpenedFile>) -> bool {
        self.inode_identity().ok() == other.inode_identity().ok()
    }

    fn inode_identity(&self) -> Result<(usize, u64), FileSystemError> {
        Ok((self.inode.filesystem_id(), self.inode.metadata()?.inode))
    }

    /// @description 从稳定 opened-entry 链投影当前 namespace pathname。
    ///
    /// @param deleted_suffix 为 true 时按 procfs 规则为已删除链追加 ` (deleted)`；
    /// false 时已删除链返回 `NotFound`，供 getcwd 使用。
    /// @return 当前绝对路径。
    /// @errors opened-entry 链损坏、形成环或内存不足时返回明确错误。
    pub(super) fn path(&self, deleted_suffix: bool) -> Result<Vec<u8>, FileSystemError> {
        let mut components = Vec::new();
        let mut current = self.location.lock().parent.clone();
        let own = self.location.lock();
        let mut deleted = own.deleted;
        if own.parent.is_some() {
            components
                .try_reserve(1)
                .map_err(|_| FileSystemError::OutOfMemory)?;
            components.push(own.name);
        }
        drop(own);

        let mut visited = Vec::new();
        while let Some(entry) = current {
            let identity = Arc::as_ptr(&entry) as usize;
            if visited.contains(&identity) {
                return Err(FileSystemError::InvalidFileSystem);
            }
            visited
                .try_reserve(1)
                .map_err(|_| FileSystemError::OutOfMemory)?;
            visited.push(identity);
            let location = entry.location.lock();
            deleted |= location.deleted;
            if location.parent.is_some() {
                components
                    .try_reserve(1)
                    .map_err(|_| FileSystemError::OutOfMemory)?;
                components.push(location.name);
            }
            current = location.parent.clone();
        }
        if deleted && !deleted_suffix {
            return Err(FileSystemError::NotFound);
        }

        let component_bytes = components
            .iter()
            .try_fold(0usize, |total, name| total.checked_add(name.bytes().len()));
        let suffix = usize::from(deleted) * b" (deleted)".len();
        let capacity = component_bytes
            .and_then(|total| total.checked_add(components.len().max(1)))
            .and_then(|total| total.checked_add(suffix))
            .ok_or(FileSystemError::InvalidFileSystem)?;
        let mut path = Vec::new();
        path.try_reserve_exact(capacity)
            .map_err(|_| FileSystemError::OutOfMemory)?;
        path.push(b'/');
        for component in components.iter().rev() {
            if path.len() > 1 {
                path.push(b'/');
            }
            path.extend_from_slice(component.bytes());
        }
        if deleted {
            path.extend_from_slice(b" (deleted)");
        }
        Ok(path)
    }
}

impl VirtualFileSystem {
    pub(super) fn mark_unlinked(
        &self,
        parent: &Arc<OpenedFile>,
        name: &[u8],
        inode_identity: (usize, u64),
    ) {
        let mut registry = self.opened.lock();
        registry.retain(|entry| {
            let Some(opened) = entry.upgrade() else {
                return false;
            };
            if opened.matches(parent, name, inode_identity) {
                opened.mark_deleted();
            }
            true
        });
    }

    pub(super) fn move_opened_entries(
        &self,
        old_parent: &Arc<OpenedFile>,
        old_name: &[u8],
        source_identity: (usize, u64),
        new_parent: Arc<OpenedFile>,
        new_name: &[u8],
    ) {
        let mut registry = self.opened.lock();
        registry.retain(|entry| {
            let Some(opened) = entry.upgrade() else {
                return false;
            };
            if opened.matches(old_parent, old_name, source_identity) {
                opened.move_to(new_parent.clone(), new_name);
            }
            true
        });
    }
}
