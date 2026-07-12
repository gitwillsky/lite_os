use alloc::sync::Arc;

use super::{AccessIdentity, FileSystemError, Inode, InodeType, VirtualFileSystem};
use crate::fs::CreateMetadata;

impl VirtualFileSystem {
    /// @description 校验 parent access、umask/setgid inheritance 后创建 inode。
    pub(crate) fn create_at(
        &self,
        start: Option<Arc<dyn Inode>>,
        path: &[u8],
        kind: InodeType,
        mode: u32,
        identity: &AccessIdentity,
    ) -> Result<Arc<dyn Inode>, FileSystemError> {
        let start = start.unwrap_or(self.root_inode()?);
        let (parent, name) = self.parent_from(start, path, identity)?;
        let parent_metadata = parent.metadata()?;
        identity.require(parent_metadata, 3)?;
        let gid = if parent_metadata.mode & 0o2000 != 0 {
            parent_metadata.gid
        } else {
            identity.gid()
        };
        let mode = mode
            | if kind == InodeType::Directory {
                parent_metadata.mode & 0o2000
            } else {
                0
            };
        parent.create(
            &name,
            kind,
            CreateMetadata {
                mode,
                uid: identity.uid(),
                gid,
            },
        )
    }

    /// @description 校验 parent access 后创建 owner-aware symbolic link。
    pub(crate) fn symlink_at(
        &self,
        start: Option<Arc<dyn Inode>>,
        path: &[u8],
        target: &[u8],
        identity: &AccessIdentity,
    ) -> Result<Arc<dyn Inode>, FileSystemError> {
        let start = start.unwrap_or(self.root_inode()?);
        let (parent, name) = self.parent_from(start, path, identity)?;
        let metadata = parent.metadata()?;
        identity.require(metadata, 3)?;
        let gid = if metadata.mode & 0o2000 != 0 {
            metadata.gid
        } else {
            identity.gid()
        };
        parent.symlink(
            &name,
            target,
            CreateMetadata {
                mode: 0o777,
                uid: identity.uid(),
                gid,
            },
        )
    }

    /// @description 执行 protected-hardlink、parent access 与 cross-mount policy。
    pub(crate) fn link_at(
        &self,
        target: Arc<dyn Inode>,
        new_start: Option<Arc<dyn Inode>>,
        new_path: &[u8],
        identity: &AccessIdentity,
    ) -> Result<(), FileSystemError> {
        let new_start = new_start.unwrap_or(self.root_inode()?);
        let (parent, name) = self.parent_from(new_start, new_path, identity)?;
        identity.require(parent.metadata()?, 3)?;
        let target_metadata = target.metadata()?;
        let safe_source = target_metadata.kind == InodeType::File
            && target_metadata.mode & 0o4000 == 0
            && target_metadata.mode & 0o2010 != 0o2010
            && identity.permits(target_metadata, 6);
        if identity.uid() != 0 && identity.uid() != target_metadata.uid && !safe_source {
            return Err(FileSystemError::PermissionDenied);
        }
        if parent.filesystem_id() != target.filesystem_id() {
            return Err(FileSystemError::CrossDevice);
        }
        parent.link(&name, target)
    }

    /// @description 执行 parent access 与 sticky-directory policy 后删除 entry。
    pub(crate) fn unlink_at(
        &self,
        start: Option<Arc<dyn Inode>>,
        path: &[u8],
        directory: bool,
        identity: &AccessIdentity,
    ) -> Result<(), FileSystemError> {
        let start = start.unwrap_or(self.root_inode()?);
        let (parent, name) = self.parent_from(start, path, identity)?;
        let parent_metadata = parent.metadata()?;
        identity.require(parent_metadata, 3)?;
        let target = parent.find_child(&name)?.metadata()?;
        if sticky_denied(
            parent_metadata.mode,
            parent_metadata.uid,
            target.uid,
            identity.uid(),
        ) {
            return Err(FileSystemError::PermissionDenied);
        }
        parent.unlink(&name, directory)
    }

    /// @description 对源/目标 parent 与 sticky owner 统一授权后原子 rename。
    pub(crate) fn rename_at(
        &self,
        old_start: Option<Arc<dyn Inode>>,
        old_path: &[u8],
        new_start: Option<Arc<dyn Inode>>,
        new_path: &[u8],
        no_replace: bool,
        identity: &AccessIdentity,
    ) -> Result<(), FileSystemError> {
        let old_start = old_start.unwrap_or(self.root_inode()?);
        let new_start = new_start.unwrap_or(self.root_inode()?);
        let (old_parent, old_name) = self.parent_from(old_start, old_path, identity)?;
        let (new_parent, new_name) = self.parent_from(new_start, new_path, identity)?;
        let old_metadata = old_parent.metadata()?;
        let new_metadata = new_parent.metadata()?;
        identity.require(old_metadata, 3)?;
        identity.require(new_metadata, 3)?;
        let source = old_parent.find_child(&old_name)?.metadata()?;
        if sticky_denied(
            old_metadata.mode,
            old_metadata.uid,
            source.uid,
            identity.uid(),
        ) {
            return Err(FileSystemError::PermissionDenied);
        }
        if let Ok(target) = new_parent.find_child(&new_name) {
            let target = target.metadata()?;
            if sticky_denied(
                new_metadata.mode,
                new_metadata.uid,
                target.uid,
                identity.uid(),
            ) {
                return Err(FileSystemError::PermissionDenied);
            }
        }
        if old_parent.filesystem_id() != new_parent.filesystem_id() {
            return Err(FileSystemError::CrossDevice);
        }
        old_parent.rename(&old_name, new_metadata.inode, &new_name, no_replace)
    }
}

fn sticky_denied(mode: u32, directory_uid: u32, target_uid: u32, caller_uid: u32) -> bool {
    mode & 0o1000 != 0 && caller_uid != 0 && caller_uid != directory_uid && caller_uid != target_uid
}
