use alloc::sync::Arc;

use super::{AccessIdentity, FileSystemError, Inode, InodeType, OpenedFile, VirtualFileSystem};
use crate::fs::CreateMetadata;

impl VirtualFileSystem {
    /// @description 校验 parent access、umask/setgid inheritance 后创建 inode。
    pub(crate) fn create_at(
        &self,
        start: Option<Arc<OpenedFile>>,
        path: &[u8],
        kind: InodeType,
        mode: u32,
        identity: &AccessIdentity,
    ) -> Result<Arc<OpenedFile>, FileSystemError> {
        let _namespace = self
            .namespace_mutation
            .lock()
            .map_err(|_| FileSystemError::OutOfMemory)?;
        let start = match start {
            Some(start) => start,
            None => self.root_opened()?,
        };
        self.create_at_locked(start, path, kind, mode, identity)
    }

    /// @description 在 namespace mutation transaction 中原子打开或创建普通文件。
    ///
    /// @param start relative path 的起始 opened entry；absolute path 会由 VFS 从 root 解析。
    /// @param path 已从 userspace 复制并验证的 pathname bytes。
    /// @param mode 创建时已经过 caller umask 收敛的 permission bits。
    /// @param identity 本次 operation 的 effective credential snapshot。
    /// @param exclusive true 表示已存在时返回 `AlreadyExists`，对应 `O_EXCL`。
    /// @return 已存在或本事务新建文件的唯一 opened entry。
    /// @errors 传播 lookup、permission、allocation 与 filesystem mutation 错误。
    pub(crate) fn open_or_create_file_at(
        &self,
        start: Option<Arc<OpenedFile>>,
        path: &[u8],
        mode: u32,
        identity: &AccessIdentity,
        exclusive: bool,
    ) -> Result<Arc<OpenedFile>, FileSystemError> {
        let _namespace = self
            .namespace_mutation
            .lock()
            .map_err(|_| FileSystemError::OutOfMemory)?;
        let start = match start {
            Some(start) => start,
            None => self.root_opened()?,
        };
        match self.open_file_at(Some(start.clone()), path, identity) {
            Ok(_) if exclusive => Err(FileSystemError::AlreadyExists),
            Ok(opened) => Ok(opened),
            Err(FileSystemError::NotFound) if path.last() == Some(&b'/') => {
                Err(FileSystemError::NotDirectory)
            }
            Err(FileSystemError::NotFound) => {
                self.create_at_locked(start, path, InodeType::File, mode, identity)
            }
            Err(error) => Err(error),
        }
    }

    /// namespace mutation lock 已由 caller 唯一持有的 create commit。
    fn create_at_locked(
        &self,
        start: Arc<OpenedFile>,
        path: &[u8],
        kind: InodeType,
        mode: u32,
        identity: &AccessIdentity,
    ) -> Result<Arc<OpenedFile>, FileSystemError> {
        // `/` 是已存在的 namespace entry；若继续交给 parent/name 分割，空末项会被
        // 误报为 EINVAL，导致标准 `mkdir -p /absolute/path` 无法跳过 root。
        if path.iter().all(|byte| *byte == b'/') {
            return Err(FileSystemError::AlreadyExists);
        }
        let (parent, name) = self.parent_from(start, path, identity)?;
        if matches!(name.as_slice(), b"." | b"..") {
            return Err(FileSystemError::AlreadyExists);
        }
        let parent_inode = parent.inode();
        let parent_metadata = parent_inode.metadata()?;
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
        let inode = parent_inode.create(
            &name,
            kind,
            CreateMetadata {
                mode,
                uid: identity.uid(),
                gid,
            },
        )?;
        self.opened
            .register(OpenedFile::child(inode, parent, &name)?)
    }

    /// @description 校验 parent access 后创建 owner-aware symbolic link。
    pub(crate) fn symlink_at(
        &self,
        start: Option<Arc<OpenedFile>>,
        path: &[u8],
        target: &[u8],
        identity: &AccessIdentity,
    ) -> Result<Arc<dyn Inode>, FileSystemError> {
        let _namespace = self
            .namespace_mutation
            .lock()
            .map_err(|_| FileSystemError::OutOfMemory)?;
        let start = match start {
            Some(start) => start,
            None => self.root_opened()?,
        };
        let (parent, name) = self.parent_from(start, path, identity)?;
        let parent_inode = parent.inode();
        let metadata = parent_inode.metadata()?;
        identity.require(metadata, 3)?;
        let gid = if metadata.mode & 0o2000 != 0 {
            metadata.gid
        } else {
            identity.gid()
        };
        parent_inode.symlink(
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
        new_start: Option<Arc<OpenedFile>>,
        new_path: &[u8],
        identity: &AccessIdentity,
    ) -> Result<(), FileSystemError> {
        let _namespace = self
            .namespace_mutation
            .lock()
            .map_err(|_| FileSystemError::OutOfMemory)?;
        let new_start = match new_start {
            Some(start) => start,
            None => self.root_opened()?,
        };
        let (parent, name) = self.parent_from(new_start, new_path, identity)?;
        let parent_inode = parent.inode();
        identity.require(parent_inode.metadata()?, 3)?;
        let target_metadata = target.metadata()?;
        let safe_source = target_metadata.kind == InodeType::File
            && target_metadata.mode & 0o4000 == 0
            && target_metadata.mode & 0o2010 != 0o2010
            && identity.permits(target_metadata, 6);
        if identity.uid() != 0 && identity.uid() != target_metadata.uid && !safe_source {
            return Err(FileSystemError::PermissionDenied);
        }
        if parent_inode.filesystem_id() != target.filesystem_id() {
            return Err(FileSystemError::CrossDevice);
        }
        parent_inode.link(&name, target)
    }

    /// @description 执行 parent access 与 sticky-directory policy 后删除 entry。
    pub(crate) fn unlink_at(
        &self,
        start: Option<Arc<OpenedFile>>,
        path: &[u8],
        directory: bool,
        identity: &AccessIdentity,
    ) -> Result<(), FileSystemError> {
        let _namespace = self
            .namespace_mutation
            .lock()
            .map_err(|_| FileSystemError::OutOfMemory)?;
        let start = match start {
            Some(start) => start,
            None => self.root_opened()?,
        };
        let (parent, name) = self.parent_from(start, path, identity)?;
        let parent_inode = parent.inode();
        let parent_metadata = parent_inode.metadata()?;
        identity.require(parent_metadata, 3)?;
        let target_inode = parent_inode.find_child(&name)?;
        let target = target_inode.metadata()?;
        if self.is_mount_point(&target_inode) {
            return Err(FileSystemError::Busy);
        }
        if sticky_denied(
            parent_metadata.mode,
            parent_metadata.uid,
            target.uid,
            identity.uid(),
        ) {
            return Err(FileSystemError::PermissionDenied);
        }
        parent_inode.unlink(&name, directory)?;
        self.opened.mark_unlinked(
            (parent_inode.filesystem_id(), parent_metadata.inode),
            &name,
            (target_inode.filesystem_id(), target.inode),
        );
        Ok(())
    }

    /// @description 对源/目标 parent 与 sticky owner 统一授权后原子 rename。
    pub(crate) fn rename_at(
        &self,
        old_start: Option<Arc<OpenedFile>>,
        old_path: &[u8],
        new_start: Option<Arc<OpenedFile>>,
        new_path: &[u8],
        no_replace: bool,
        identity: &AccessIdentity,
    ) -> Result<(), FileSystemError> {
        let _namespace = self
            .namespace_mutation
            .lock()
            .map_err(|_| FileSystemError::OutOfMemory)?;
        let old_start = match old_start {
            Some(start) => start,
            None => self.root_opened()?,
        };
        let new_start = match new_start {
            Some(start) => start,
            None => self.root_opened()?,
        };
        let (old_parent, old_name) = self.parent_from(old_start, old_path, identity)?;
        let (new_parent, new_name) = self.parent_from(new_start, new_path, identity)?;
        let old_parent_inode = old_parent.inode();
        let new_parent_inode = new_parent.inode();
        let old_metadata = old_parent_inode.metadata()?;
        let new_metadata = new_parent_inode.metadata()?;
        identity.require(old_metadata, 3)?;
        identity.require(new_metadata, 3)?;
        let source_inode = old_parent_inode.find_child(&old_name)?;
        let source = source_inode.metadata()?;
        if self.is_mount_point(&source_inode) {
            return Err(FileSystemError::Busy);
        }
        if sticky_denied(
            old_metadata.mode,
            old_metadata.uid,
            source.uid,
            identity.uid(),
        ) {
            return Err(FileSystemError::PermissionDenied);
        }
        let target = new_parent_inode.find_child(&new_name).ok();
        if let Some(target) = &target {
            if self.is_mount_point(target) {
                return Err(FileSystemError::Busy);
            }
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
        if old_parent_inode.filesystem_id() != new_parent_inode.filesystem_id() {
            return Err(FileSystemError::CrossDevice);
        }
        let source_identity = (source_inode.filesystem_id(), source.inode);
        if let Some(target) = &target
            && (target.filesystem_id(), target.metadata()?.inode) == source_identity
            && !no_replace
        {
            return Ok(());
        }
        let replaced_identity = target
            .as_ref()
            .map(|target| Ok((target.filesystem_id(), target.metadata()?.inode)))
            .transpose()?;
        old_parent_inode.rename(&old_name, new_metadata.inode, &new_name, no_replace)?;
        if let Some(identity) = replaced_identity {
            self.opened.mark_unlinked(
                (new_parent_inode.filesystem_id(), new_metadata.inode),
                &new_name,
                identity,
            );
        }
        self.opened.move_entries(
            (old_parent_inode.filesystem_id(), old_metadata.inode),
            &old_name,
            source_identity,
            new_parent,
            (new_parent_inode.filesystem_id(), new_metadata.inode),
            &new_name,
        );
        Ok(())
    }
}

fn sticky_denied(mode: u32, directory_uid: u32, target_uid: u32, caller_uid: u32) -> bool {
    mode & 0o1000 != 0 && caller_uid != 0 && caller_uid != directory_uid && caller_uid != target_uid
}
