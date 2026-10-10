use alloc::sync::Arc;

use super::{
    AccessIdentity, FileSystemError, Inode, InodeType, LookupOrigin, OpenedFile, VirtualFileSystem,
};
use crate::fs::CreateMetadata;
use crate::fs::device::DeviceNumber;

impl VirtualFileSystem {
    /// 校验 parent access、umask/setgid inheritance 后创建 inode。
    pub(crate) fn create_at(
        &self,
        start: LookupOrigin,
        path: &[u8],
        kind: InodeType,
        mode: u32,
        identity: &AccessIdentity,
    ) -> Result<Arc<OpenedFile>, FileSystemError> {
        self.mknod_at(start, path, kind, mode, None, identity)
    }

    /// 同 [`Self::create_at`]，并支持 FIFO 与设备节点（`device` 只用于设备节点）。
    pub(crate) fn mknod_at(
        &self,
        start: LookupOrigin,
        path: &[u8],
        kind: InodeType,
        mode: u32,
        device: Option<DeviceNumber>,
        identity: &AccessIdentity,
    ) -> Result<Arc<OpenedFile>, FileSystemError> {
        let namespace = self.namespace()?;
        namespace.create_entry(start, path, kind, mode, device, identity)
    }

    /// 在 namespace mutation transaction 中原子打开或创建普通文件。
    ///
    /// # Parameters
    ///
    /// - `start`: relative path 的起始 opened entry；absolute path 会由 VFS 从 root 解析。
    /// - `path`: 已从 userspace 复制并验证的 pathname bytes。
    /// - `mode`: 创建时已经过 caller umask 收敛的 permission bits。
    /// - `identity`: 本次 operation 的 effective credential snapshot。
    /// - `exclusive`: true 表示已存在时返回 `AlreadyExists`，对应 `O_EXCL`。
    /// - `no_follow`: true 保留末项 link，由 open 权限层返回 ELOOP。
    ///
    /// # Returns
    ///
    /// 已存在或本事务新建文件的唯一 opened entry 与是否由本事务创建的结果。
    ///
    /// # Errors
    ///
    /// 传播 lookup、permission、allocation 与 filesystem mutation 错误。
    pub(crate) fn open_or_create_file_at(
        &self,
        start: LookupOrigin,
        path: &[u8],
        mode: u32,
        identity: &AccessIdentity,
        exclusive: bool,
        no_follow: bool,
    ) -> Result<(Arc<OpenedFile>, bool), FileSystemError> {
        let namespace = self.namespace()?;
        match namespace.resolve_for_create(start, path, exclusive || no_follow, identity)? {
            super::path_lookup::PathLookup::Existing(_) if exclusive => {
                Err(FileSystemError::AlreadyExists)
            }
            super::path_lookup::PathLookup::Existing(opened) => {
                if opened.inode().inode_type() == InodeType::Directory {
                    Err(FileSystemError::IsDirectory)
                } else {
                    Ok((opened, false))
                }
            }
            super::path_lookup::PathLookup::Missing { parent, name } => namespace
                .create_child(parent, &name, InodeType::File, mode, None, identity)
                .map(|opened| (opened, true)),
        }
    }

    /// 校验 parent access 后创建 owner-aware symbolic link。
    pub(crate) fn symlink_at(
        &self,
        start: LookupOrigin,
        path: &[u8],
        target: &[u8],
        identity: &AccessIdentity,
    ) -> Result<Arc<dyn Inode>, FileSystemError> {
        let namespace = self.namespace()?;
        let (parent, name) = namespace.parent_from(start, path, identity)?;
        let parent_inode = parent.inode();
        let _writer = parent.begin_write()?;
        let metadata = parent_inode.metadata()?;
        parent_inode.require_access(identity, 3)?;
        let gid = if metadata.mode & 0o2000 != 0 {
            metadata.gid
        } else {
            identity.gid()
        };
        let link = parent_inode.symlink(
            &name,
            target,
            CreateMetadata {
                mode: 0o777,
                uid: identity.uid(),
                gid,
            },
        )?;
        crate::fs::notify_entry(parent_inode.as_ref(), &name, crate::fs::IN_CREATE, false, 0);
        Ok(link)
    }

    /// 执行 protected-hardlink、parent access 与 cross-mount policy。
    pub(crate) fn link_at(
        &self,
        target: Arc<OpenedFile>,
        new_start: LookupOrigin,
        new_path: &[u8],
        identity: &AccessIdentity,
    ) -> Result<(), FileSystemError> {
        let namespace = self.namespace()?;
        let (parent, name) = namespace.parent_from(new_start, new_path, identity)?;
        let parent_inode = parent.inode();
        let _writer = parent.begin_write()?;
        parent_inode.require_access(identity, 3)?;
        let target_inode = target.inode();
        let target_metadata = target_inode.metadata()?;
        let safe_source = target_metadata.kind == InodeType::File
            && target_metadata.mode & 0o4000 == 0
            && target_metadata.mode & 0o2010 != 0o2010
            && identity.permits(target_metadata, 6);
        if !identity.inode_capability(
            target_metadata.uid,
            target_metadata.gid,
            syscall_abi::capability::Capability::FOWNER,
        ) && identity.uid() != target_metadata.uid
            && !safe_source
        {
            return Err(FileSystemError::PermissionDenied);
        }
        if !parent.same_mount(&target) {
            return Err(FileSystemError::CrossDevice);
        }
        parent_inode.link(&name, target_inode.clone())?;
        crate::fs::notify_entry(parent_inode.as_ref(), &name, crate::fs::IN_CREATE, false, 0);
        // link count 变化是 inode 自己的属性变化。
        crate::fs::notify_self(target_inode.as_ref(), crate::fs::IN_ATTRIB);
        Ok(())
    }

    /// 执行 parent access 与 sticky-directory policy 后删除 entry。
    pub(crate) fn unlink_at(
        &self,
        start: LookupOrigin,
        path: &[u8],
        directory: bool,
        identity: &AccessIdentity,
    ) -> Result<(), FileSystemError> {
        let namespace = self.namespace()?;
        let (parent, name) = namespace.parent_from(start, path, identity)?;
        let parent_inode = parent.inode();
        let _writer = parent.begin_write()?;
        let parent_metadata = parent_inode.metadata()?;
        parent_inode.require_access(identity, 3)?;
        let target_inode = parent_inode.find_child(&name)?;
        let target = target_inode.metadata()?;
        // 整个 unlink 持同一 canonical path；已有 OFD/cwd 继续 pin 同一 dentry。
        let target_path = OpenedFile::child(target_inode.clone(), parent.clone(), &name)?;
        if self.is_mount_point(&target_path) {
            return Err(FileSystemError::Busy);
        }
        if !identity.may_remove_entry(parent_metadata, target.uid, target.gid) {
            return Err(FileSystemError::PermissionDenied);
        }
        parent_inode.unlink(&name, directory)?;
        // Linux vfs_unlink：link-count ATTRIB 先于 d_delete_notify 的目录 DELETE。
        crate::fs::notify_self(target_inode.as_ref(), crate::fs::IN_ATTRIB);
        self.dentries.mark_unlinked(
            (parent_inode.filesystem_id(), parent_metadata.inode),
            &name,
            (target_inode.filesystem_id(), target.inode),
        );
        let is_directory = target.kind == InodeType::Directory;
        crate::fs::notify_entry(
            parent_inode.as_ref(),
            &name,
            crate::fs::IN_DELETE,
            is_directory,
            0,
        );
        // 最后 dput 独占 cache 退休。先解锁并归还临时 inode pin，否则 cache + 临时
        // pin + dentry 会让最后 dput 误判仍有使用者，且不再有下一次释放触发回收。
        drop(namespace);
        drop(target_inode);
        drop(target_path);
        Ok(())
    }

    /// 对源/目标 parent 与 sticky owner 统一授权后原子 rename。
    pub(crate) fn rename_at(
        &self,
        old_start: LookupOrigin,
        old_path: &[u8],
        new_start: LookupOrigin,
        new_path: &[u8],
        no_replace: bool,
        identity: &AccessIdentity,
    ) -> Result<(), FileSystemError> {
        let namespace = self.namespace()?;
        let (old_parent, old_name) = namespace.parent_from(old_start, old_path, identity)?;
        let (new_parent, new_name) = namespace.parent_from(new_start, new_path, identity)?;
        let old_parent_inode = old_parent.inode();
        let new_parent_inode = new_parent.inode();
        let _old_writer = old_parent.begin_write()?;
        let _new_writer = new_parent.begin_write()?;
        let old_metadata = old_parent_inode.metadata()?;
        let new_metadata = new_parent_inode.metadata()?;
        old_parent_inode.require_access(identity, 3)?;
        new_parent_inode.require_access(identity, 3)?;
        let source_inode = old_parent_inode.find_child(&old_name)?;
        let source = source_inode.metadata()?;
        let source_path = OpenedFile::child(source_inode.clone(), old_parent.clone(), &old_name)?;
        if self.is_mount_point(&source_path) {
            return Err(FileSystemError::Busy);
        }
        if !identity.may_remove_entry(old_metadata, source.uid, source.gid) {
            return Err(FileSystemError::PermissionDenied);
        }
        let target = new_parent_inode.find_child(&new_name).ok();
        let target_path = target
            .as_ref()
            .map(|target| OpenedFile::child(target.clone(), new_parent.clone(), &new_name))
            .transpose()?;
        if let Some(target) = &target {
            if self.is_mount_point(target_path.as_ref().expect("actual rename victim path")) {
                return Err(FileSystemError::Busy);
            }
            let target = target.metadata()?;
            if !identity.may_remove_entry(new_metadata, target.uid, target.gid) {
                return Err(FileSystemError::PermissionDenied);
            }
        }
        if !old_parent.same_mount(&new_parent) {
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
        let is_directory = source.kind == InodeType::Directory;
        let cookie = crate::fs::next_cookie();
        crate::fs::notify_entry(
            old_parent_inode.as_ref(),
            &old_name,
            crate::fs::IN_MOVED_FROM,
            is_directory,
            cookie,
        );
        crate::fs::notify_entry(
            new_parent_inode.as_ref(),
            &new_name,
            crate::fs::IN_MOVED_TO,
            is_directory,
            cookie,
        );
        if let Some(target) = &target {
            crate::fs::notify_self(target.as_ref(), crate::fs::IN_ATTRIB);
        }
        crate::fs::notify_self(source_inode.as_ref(), crate::fs::IN_MOVE_SELF);
        if let Some(identity) = replaced_identity {
            self.dentries.mark_unlinked(
                (new_parent_inode.filesystem_id(), new_metadata.inode),
                &new_name,
                identity,
            );
        }
        self.dentries.move_entry(
            (old_parent_inode.filesystem_id(), old_metadata.inode),
            &old_name,
            source_identity,
            new_parent.dentry.clone(),
            (new_parent_inode.filesystem_id(), new_metadata.inode),
            &new_name,
        );
        // rename victim 与 unlink 使用同一个 final-dput cache owner；不能另按 Arc 数
        // 提前丢弃仍被已打开 pathname 使用的 dirty pages。
        drop(namespace);
        drop(target);
        drop(target_path);
        Ok(())
    }
}

impl super::NamespaceAccess<'_> {
    /// namespace mutation lock 已由 caller 唯一持有的 create commit。
    pub(super) fn create_entry(
        &self,
        start: LookupOrigin,
        path: &[u8],
        kind: InodeType,
        mode: u32,
        device: Option<DeviceNumber>,
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
        self.create_child(parent, &name, kind, mode, device, identity)
    }

    /// 消费同一 namespace transaction 的实际 parent/name；不重走未展开的 pathname。
    fn create_child(
        &self,
        parent: Arc<OpenedFile>,
        name: &[u8],
        kind: InodeType,
        mode: u32,
        device: Option<DeviceNumber>,
        identity: &AccessIdentity,
    ) -> Result<Arc<OpenedFile>, FileSystemError> {
        let parent_inode = parent.inode();
        let _writer = parent.begin_write()?;
        let parent_metadata = parent_inode.metadata()?;
        parent_inode.require_access(identity, 3)?;
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
        let metadata = CreateMetadata {
            mode,
            uid: identity.uid(),
            gid,
        };
        let inode = match kind {
            InodeType::Fifo | InodeType::CharacterDevice | InodeType::BlockDevice => {
                parent_inode.mknod(name, kind, metadata, device)?
            }
            _ => parent_inode.create(name, kind, metadata)?,
        };
        crate::fs::notify_entry(
            parent_inode.as_ref(),
            name,
            crate::fs::IN_CREATE,
            kind == InodeType::Directory,
            0,
        );
        OpenedFile::child(inode, parent, name)
    }
}
