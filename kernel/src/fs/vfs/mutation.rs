use alloc::sync::Arc;

use super::{
    AccessIdentity, FileSystemError, Inode, InodeType, LookupOrigin, OpenedFile, VirtualFileSystem,
    path_lookup::{CreatedEntry, PathLookup},
};
use crate::fs::CreateMetadata;
use crate::fs::DeviceNumber;
use crate::sync::TaskMutexWaitPreparation;

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
        // `/` 是已存在的 namespace entry；若继续交给 parent/name 分割，空末项会被
        // 误报为 EINVAL，导致标准 `mkdir -p /absolute/path` 无法跳过 root。
        if path.iter().all(|byte| *byte == b'/') {
            return Err(FileSystemError::AlreadyExists);
        }
        let (parent, name) = self.parent_from(start, path, identity)?;
        if matches!(name.as_slice(), b"." | b"..") {
            return Err(FileSystemError::AlreadyExists);
        }
        // Linux filename_create：父目录独占持有贯穿存在检查与创建。
        let _directory = parent.dentry.lock_exclusive()?;
        live_directory(&parent)?;
        self.create_child(parent.clone(), &name, kind, mode, device, identity, None)
            .map(|(opened, _)| opened)
    }

    /// 原子打开或创建普通文件：末项的查找与创建在父目录独占持有内完成（Linux `lookup_open`）。
    ///
    /// # Parameters
    ///
    /// - `start`: relative path 的起始 opened entry；absolute path 会由 VFS 从 root 解析。
    /// - `path`: 已从 userspace 复制并验证的 pathname bytes。
    /// - `mode`: 创建时已经过 caller umask 收敛的 permission bits。
    /// - `identity`: 本次 operation 的 effective credential snapshot。
    /// - `open_flags`: 本次 open 的 status flags：`O_EXCL` 时已存在返回 `AlreadyExists`，`O_NOFOLLOW`
    ///   保留末项 link（由 open 权限层返回 ELOOP）；filesystem 支持原子创建打开时随创建一起打开。
    ///
    /// # Returns
    ///
    /// 已存在或本事务新建文件的唯一 opened entry、是否由本事务创建，以及 filesystem 原子创建时
    /// 已打开的 backend（Linux `atomic_open`）；为 `None` 时调用者照常 `open_backend`。
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
        open_flags: u32,
    ) -> Result<(Arc<OpenedFile>, bool, Option<crate::fs::InodeOpen>), FileSystemError> {
        let exclusive = open_flags & syscall_abi::open::O_EXCL != 0;
        let no_follow = open_flags & crate::fs::O_NOFOLLOW != 0;
        let mut create = |parent: Arc<OpenedFile>, name: &[u8]| {
            self.create_child(
                parent,
                name,
                InodeType::File,
                mode,
                None,
                identity,
                Some(open_flags),
            )
        };
        match self.resolve_for_create(start, path, exclusive || no_follow, identity, &mut create)? {
            PathLookup::Existing(_) if exclusive => Err(FileSystemError::AlreadyExists),
            PathLookup::Existing(opened) => {
                if opened.inode().inode_type() == InodeType::Directory {
                    Err(FileSystemError::IsDirectory)
                } else {
                    Ok((opened, false, None))
                }
            }
            PathLookup::Created((opened, backend)) => Ok((opened, true, backend)),
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
        let (parent, name) = self.parent_from(start, path, identity)?;
        let _directory = parent.dentry.lock_exclusive()?;
        live_directory(&parent)?;
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
        let (parent, name) = self.parent_from(new_start, new_path, identity)?;
        let _directory = parent.dentry.lock_exclusive()?;
        live_directory(&parent)?;
        let parent_inode = parent.inode();
        let _writer = parent.begin_write()?;
        parent_inode.require_access(identity, 3)?;
        if !parent.same_mount(&target) {
            return Err(FileSystemError::CrossDevice);
        }
        let target_inode = target.inode();
        // 目录不可创建硬链接；必须先拒绝，避免 source 恰为 parent 时递归取得目录锁。
        if target_inode.inode_type() == InodeType::Directory {
            return Err(FileSystemError::PermissionDenied);
        }
        let _source = target.dentry.lock_exclusive()?;
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
        let (parent, name) = self.parent_from(start, path, identity)?;
        let directory_guard = parent.dentry.lock_exclusive()?;
        live_directory(&parent)?;
        let parent_inode = parent.inode();
        let _writer = parent.begin_write()?;
        let parent_metadata = parent_inode.metadata()?;
        parent_inode.require_access(identity, 3)?;
        let target_inode = parent_inode.find_child(&name)?;
        let target = target_inode.metadata()?;
        // 整个 unlink 持同一 canonical path；已有 OFD/cwd 继续 pin 同一 dentry。
        let target_path = OpenedFile::child(target_inode.clone(), parent.clone(), &name)?;
        // Linux vfs_unlink/vfs_rmdir 锁受害者：与挂载到它（lock_mount）及在它里面查找互斥。
        let victim_guard = target_path.dentry.lock_exclusive()?;
        if self.is_mount_point(&target_path) {
            return Err(FileSystemError::Busy);
        }
        if !identity.may_remove_entry(parent_metadata, target.uid, target.gid) {
            return Err(FileSystemError::PermissionDenied);
        }
        let mut publication =
            TaskMutexWaitPreparation::prepare().map_err(|_| FileSystemError::OutOfMemory)?;
        parent_inode.unlink(&name, directory)?;
        target_inode.unlinked(directory);
        // Linux vfs_unlink：link-count ATTRIB 先于 d_delete_notify 的目录 DELETE。
        crate::fs::notify_self(target_inode.as_ref(), crate::fs::IN_ATTRIB);
        {
            let _namespace = self.namespace_prepared(&mut publication);
            self.dentries.mark_unlinked(
                (parent_inode.filesystem_id(), parent_metadata.inode),
                &name,
                (target_inode.filesystem_id(), target.inode),
            );
        }
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
        drop(victim_guard);
        drop(directory_guard);
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
        let (old_parent, old_name) = self.parent_from(old_start, old_path, identity)?;
        let (new_parent, new_name) = self.parent_from(new_start, new_path, identity)?;
        if !old_parent.same_mount(&new_parent) {
            return Err(FileSystemError::CrossDevice);
        }
        let old_parent_inode = old_parent.inode();
        let new_parent_inode = new_parent.inode();
        let _old_writer = old_parent.begin_write()?;
        let _new_writer = new_parent.begin_write()?;
        // Linux lock_rename：同一父目录只锁一次；跨目录先取 filesystem rename 锁冻结祖先关系，
        // 再先祖先后后代地锁两个父目录（互不为祖先时任意固定顺序，跨目录 rename 已被串行）。
        let cross = !Arc::ptr_eq(&old_parent.dentry, &new_parent.dentry);
        let rename = if cross {
            let filesystem = &old_parent
                .mount
                .as_ref()
                .ok_or(FileSystemError::InvalidOperation)?
                .filesystem;
            Some(
                filesystem
                    .rename
                    .lock()
                    .map_err(|_| FileSystemError::OutOfMemory)?,
            )
        } else {
            None
        };
        let (first, second) = if cross && new_parent.dentry.is_ancestor_of(&old_parent.dentry) {
            (&new_parent, Some(&old_parent))
        } else {
            (&old_parent, cross.then_some(&new_parent))
        };
        let first_directory = first.dentry.lock_exclusive()?;
        let second_directory = second
            .map(|parent| parent.dentry.lock_exclusive())
            .transpose()?;
        live_directory(&old_parent)?;
        live_directory(&new_parent)?;
        let old_metadata = old_parent_inode.metadata()?;
        let new_metadata = new_parent_inode.metadata()?;
        old_parent_inode.require_access(identity, 3)?;
        new_parent_inode.require_access(identity, 3)?;
        let source_inode = old_parent_inode.find_child(&old_name)?;
        let source = source_inode.metadata()?;
        let source_path = OpenedFile::child(source_inode.clone(), old_parent.clone(), &old_name)?;
        // Linux lock_rename 的 trap：源是新父目录自身或其祖先（移入自身子树）为 EINVAL，
        // 目标是旧父目录自身或其祖先为 ENOTEMPTY；必须先于锁受害者判定，否则会逆着祖先序加锁。
        if cross && source_path.dentry.is_ancestor_of(&new_parent.dentry) {
            return Err(FileSystemError::InvalidOperation);
        }
        if !identity.may_remove_entry(old_metadata, source.uid, source.gid) {
            return Err(FileSystemError::PermissionDenied);
        }
        let target = match new_parent_inode.find_child(&new_name) {
            Ok(target) => Some(target),
            Err(FileSystemError::NotFound) => None,
            Err(error) => return Err(error),
        };
        // 不支持 RENAME_EXCHANGE；目录与非目录不能互相覆盖。先拒绝才不会逆序锁不同种类的受害者。
        if let Some(target) = &target
            && (source.kind == InodeType::Directory)
                != (target.inode_type() == InodeType::Directory)
        {
            return Err(if source.kind == InodeType::Directory {
                FileSystemError::NotDirectory
            } else {
                FileSystemError::IsDirectory
            });
        }
        let target_path = target
            .as_ref()
            .map(|target| OpenedFile::child(target.clone(), new_parent.clone(), &new_name))
            .transpose()?;
        if cross
            && target_path
                .as_ref()
                .is_some_and(|target| target.dentry.is_ancestor_of(&old_parent.dentry))
        {
            return Err(FileSystemError::DirectoryNotEmpty);
        }
        // 受害者在两个父目录之后按地址序加锁（Linux lock_two_nondirectories）；同名同 inode 只锁一次。
        let mut victims = [
            Some(&source_path.dentry),
            target_path.as_ref().map(|t| &t.dentry),
        ];
        if victims[1].is_some_and(|target| Arc::ptr_eq(target, &source_path.dentry)) {
            victims[1] = None;
        }
        victims.sort_by_key(|victim| victim.map(Arc::as_ptr));
        let first_victim = victims[0]
            .map(|dentry| dentry.lock_exclusive())
            .transpose()?;
        let second_victim = victims[1]
            .map(|dentry| dentry.lock_exclusive())
            .transpose()?;
        // 挂载点检查必须在取得受害者锁之后；否则 lock_mount 可在检查与 rename 之间插入挂载。
        if self.is_mount_point(&source_path) {
            return Err(FileSystemError::Busy);
        }
        if let Some(target) = &target {
            if self.is_mount_point(target_path.as_ref().expect("actual rename victim path")) {
                return Err(FileSystemError::Busy);
            }
            let target = target.metadata()?;
            if !identity.may_remove_entry(new_metadata, target.uid, target.gid) {
                return Err(FileSystemError::PermissionDenied);
            }
        }
        let source_identity = (source_inode.filesystem_id(), source.inode);
        if let Some(target) = &target
            && (target.filesystem_id(), target.identity()?.1) == source_identity
            && !no_replace
        {
            return Ok(());
        }
        let replaced_identity = target
            .as_ref()
            .map(|target| Ok((target.filesystem_id(), target.identity()?.1)))
            .transpose()?;
        let mut publication =
            TaskMutexWaitPreparation::prepare().map_err(|_| FileSystemError::OutOfMemory)?;
        old_parent_inode.rename(&old_name, new_metadata.inode, &new_name, no_replace)?;
        if let Some(target) = &target {
            target.unlinked(target.inode_type() == InodeType::Directory);
        }
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
        {
            let _namespace = self.namespace_prepared(&mut publication);
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
        }
        // rename victim 与 unlink 使用同一个 final-dput cache owner；不能另按 Arc 数
        // 提前丢弃仍被已打开 pathname 使用的 dirty pages。先解锁再做最后 dput。
        drop((first_victim, second_victim));
        drop((first_directory, second_directory, rename));
        drop(target);
        drop(target_path);
        Ok(())
    }
}

impl VirtualFileSystem {
    /// 在 caller 已独占持有的父目录里创建 `name`；消费 walk 得到的实际 parent，不重走 pathname。
    ///
    /// `open` 为 `Some(flags)` 时（`open(O_CREAT)` 创建普通文件）先请 filesystem 原子创建并打开，
    /// 返回其 backend；不支持时退回 `create`，backend 为 `None`。
    #[expect(
        clippy::too_many_arguments,
        reason = "同一次创建事务的独立领域输入，与 mknod_at 的参数逐项对应"
    )]
    fn create_child(
        &self,
        parent: Arc<OpenedFile>,
        name: &[u8],
        kind: InodeType,
        mode: u32,
        device: Option<DeviceNumber>,
        identity: &AccessIdentity,
        open: Option<u32>,
    ) -> Result<CreatedEntry, FileSystemError> {
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
        let created = match (kind, open) {
            (InodeType::File, Some(flags)) => parent_inode.create_open(name, metadata, flags)?,
            _ => None,
        };
        let (inode, backend) = match created {
            Some((inode, backend)) => (inode, Some(backend)),
            None => (
                match kind {
                    InodeType::Fifo | InodeType::CharacterDevice | InodeType::BlockDevice => {
                        parent_inode.mknod(name, kind, metadata, device)?
                    }
                    _ => parent_inode.create(name, kind, metadata)?,
                },
                None,
            ),
        };
        crate::fs::notify_entry(
            parent_inode.as_ref(),
            name,
            crate::fs::IN_CREATE,
            kind == InodeType::Directory,
            0,
        );
        OpenedFile::child(inode, parent, name).map(|opened| (opened, backend))
    }
}

/// Linux `IS_DEADDIR`：已删除的目录里不再创建、删除或改名（ENOENT）；调用者持有该目录锁。
fn live_directory(directory: &OpenedFile) -> Result<(), FileSystemError> {
    if directory.is_unlinked() {
        return Err(FileSystemError::NotFound);
    }
    Ok(())
}
