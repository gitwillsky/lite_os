//! 实际路径是 mount view + canonical filesystem dentry；不复制目录关系或按 inode 猜挂载。
use super::{
    FileSystemError, Inode, MountFlags, WriteOpen,
    dentry::{Dentry, FileName},
    mount_view::MountView,
};
use alloc::{sync::Arc, vec::Vec};

/// OFD/cwd/root 保活的真实 Linux path identity；namespace membership 独立于旧路径生命周期。
pub(crate) struct OpenedFile {
    // 所有路径（包括 allocation 失败）都先 dput 再 mntput；不能先关闭 inode 所属 filesystem。
    pub(super) dentry: Arc<Dentry>,
    pub(super) mount: Option<Arc<MountView>>,
    clear_expiry: bool,
}
#[derive(Clone, Copy)]
enum ProjectionRoot<'a> {
    Namespace(&'a OpenedFile),
    Filesystem(&'a Dentry),
}
impl OpenedFile {
    pub(super) fn at(
        mount: Arc<MountView>,
        dentry: Arc<Dentry>,
    ) -> Result<Arc<Self>, FileSystemError> {
        Arc::try_new(Self {
            mount: Some(mount),
            dentry,
            clear_expiry: true,
        })
        .map_err(|_| FileSystemError::OutOfMemory)
    }
    pub(super) fn child(
        inode: Arc<dyn Inode>,
        parent: Arc<Self>,
        name: &[u8],
    ) -> Result<Arc<Self>, FileSystemError> {
        let mount = parent
            .mount
            .as_ref()
            .ok_or(FileSystemError::InvalidOperation)?
            .clone();
        let dentry = super::vfs().dentries.admit(Dentry::named(
            inode,
            Some(parent.dentry.clone()),
            name,
        )?)?;
        Self::at(mount, dentry)
    }
    pub(in crate::fs) fn anonymous(
        filesystem: crate::fs::anonymous::PseudoFileSystem,
        inode: Arc<dyn Inode>,
        label: Vec<u8>,
    ) -> Result<Arc<Self>, FileSystemError> {
        let dentry = Dentry::anonymous(inode, label)?;
        filesystem.open(dentry)
    }
    /// pseudo magic dentry借同一实际internal mount；每次open只创建path shell。
    /// 分配失败不改变dentry identity、filesystem owner或namespace membership。
    pub(in crate::fs) fn with_dentry(
        &self,
        dentry: Arc<Dentry>,
    ) -> Result<Arc<Self>, FileSystemError> {
        Self::at(
            self.mount
                .as_ref()
                .ok_or(FileSystemError::InvalidOperation)?
                .clone(),
            dentry,
        )
    }
    /// path_umount 消费调用者独有 path，mntput_no_expire 不清除刚设定的 expiry mark。
    pub(in crate::fs) fn preserve_mount_expiry(&mut self) {
        self.clear_expiry = false;
    }
    pub(crate) fn inode(&self) -> Arc<dyn Inode> {
        self.dentry.inode()
    }
    pub(crate) fn mount_flags(&self) -> MountFlags {
        self.mount
            .as_ref()
            .map_or_else(MountFlags::default, |view| view.flags())
    }
    /// fdinfo 与 `statx` 使用实际 vfsmount 的稳定 identity，包括所有内部 pseudo 路径。
    pub(crate) fn mount_id(&self) -> u64 {
        self.mount.as_ref().map_or(0, |view| view.id)
    }
    /// mnt_may_suid：只有 caller namespace 的实际 mount 且 caller 在 superblock owning userNS 内才可信。
    /// 旧 namespace FD/proc magic path 与匿名 hidden mount 一律不授予 set-ID/file capabilities。
    pub(crate) fn may_suid(
        &self,
        namespace: &super::MountNamespace,
        user: &Arc<crate::namespace::UserNamespace>,
    ) -> bool {
        self.mount.as_ref().is_some_and(|view| {
            !view.flags().nosuid()
                && namespace.contains(view)
                && user.is_within(&view.filesystem.user)
        })
    }
    /// 已进入实际非零 regular read transaction 的锁外资源副作用，无新分配；EOF/EFAULT 同样触碰 atime。
    pub(crate) fn read_access(&self) -> ReadAccess<'_> {
        ReadAccess(self)
    }
    /// 根据本路径所属 mount 更新 inode atime；cache hit 与 miss 使用同一事实，失败不改变读取结果。
    pub(crate) fn touch_access(&self) {
        let flags = self.mount_flags();
        let inode = self.inode();
        if flags.noatime()
            || flags.nodiratime() && inode.inode_type() == super::InodeType::Directory
        {
            return;
        }
        let now = crate::timer::get_realtime_ns() / 1_000_000_000;
        if let Some(view) = &self.mount {
            let Some(_writer) = view.try_access_writer() else {
                return;
            };
            let _ = inode.touch_access(now, flags.relatime());
        } else {
            let _ = inode.touch_access(now, flags.relatime());
        }
    }
    /// 内容写入需同时持 actual mount writer 与 inode write-access，不能经别名写正在执行的文件。
    pub(crate) fn begin_content_write(&self) -> Result<ContentWrite, FileSystemError> {
        let mount = self.begin_write()?;
        let inode = super::super::InodeAccessLease::write(self.inode())?;
        Ok(ContentWrite {
            _inode: Some(inode),
            _mount: mount,
        })
    }
    pub(crate) fn begin_write(&self) -> Result<WriteOpen, FileSystemError> {
        self.mount
            .as_ref()
            .map_or(Ok(WriteOpen::unmounted()), |view| view.acquire_writer())
    }
    pub(in crate::fs) fn is_unlinked(&self) -> bool {
        self.dentry.is_unlinked()
    }
    pub(super) fn same_path(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.dentry, &other.dentry)
            && match (&self.mount, &other.mount) {
                (Some(left), Some(right)) => Arc::ptr_eq(left, right),
                (None, None) => true,
                _ => false,
            }
    }
    pub(crate) fn same_mount(&self, other: &Self) -> bool {
        match (&self.mount, &other.mount) {
            (Some(left), Some(right)) => Arc::ptr_eq(left, right),
            (None, None) => true,
            _ => false,
        }
    }
    #[cfg(test)]
    pub(super) fn inode_identity(&self) -> Result<(usize, u64), FileSystemError> {
        self.dentry.inode_identity()
    }
    /// `..` 在 mount root 从实际 attachment 回到 mountpoint 的父目录；inode 相同并不代表同一路径。
    pub(super) fn parent(&self, stop: &Arc<Self>) -> Result<Option<Arc<Self>>, FileSystemError> {
        let Some(view) = &self.mount else {
            return Ok(None);
        };
        let mut view = view.clone();
        let mut dentry = self.dentry.clone();
        loop {
            if stop
                .mount
                .as_ref()
                .is_some_and(|root| Arc::ptr_eq(root, &view))
                && Arc::ptr_eq(&stop.dentry, &dentry)
            {
                return Ok(Some(stop.clone()));
            }
            if Arc::ptr_eq(&view.root, &dentry) {
                let Some(attachment) = view.attachment() else {
                    return Ok(None);
                };
                let Some(parent) = attachment.parent.upgrade() else {
                    return Ok(None);
                };
                view = parent;
                dentry = attachment.point;
                // 叠加 mount root 可以先跨越多层 view，最后才到达 filesystem 内的父目录。
                // 每次 follow-up 都先检查 caller root，不能越过下层 chroot root 再取父目录。
                continue;
            }
            let parent = dentry.snapshot().and_then(|(parent, _, _)| parent);
            return parent.map(|parent| Self::at(view, parent)).transpose();
        }
    }
    /// inotify 的 parent 是同一 filesystem 内的 canonical dentry；mount root 不制造跨 superblock 父节点。
    pub(crate) fn watch_context(&self) -> Option<(Arc<dyn Inode>, Vec<u8>, bool)> {
        let (parent, name, unlinked) = self.dentry.snapshot()?;
        Some((parent?.inode(), copy_bytes(name.bytes()).ok()?, unlinked))
    }
    /// 在 caller root 中投影 canonical dentry 与实际 mount attachment；visited 仅检测损坏环。
    /// OOM/环/超出 Linux PATH_MAX 返回明确错误，不从保存的字符串重新解析路径。
    pub(super) fn path(
        &self,
        root: &Arc<Self>,
        deleted_suffix: bool,
    ) -> Result<Vec<u8>, FileSystemError> {
        self.project_path(root, deleted_suffix)
            .map(|(path, _)| path)
    }
    /// seq_path_root 的可达性与 pathname 来自同一次遍历；不可用字符串前缀猜 chroot 可见性。
    pub(super) fn project_path(
        &self,
        root: &Arc<Self>,
        deleted_suffix: bool,
    ) -> Result<(Vec<u8>, bool), FileSystemError> {
        self.walk_path(ProjectionRoot::Namespace(root), deleted_suffix)
    }
    /// mountinfo 的 root 是 filesystem 内 dentry 路径，不能沿 namespace attachment 展开。
    pub(super) fn filesystem_path(&self, root: &Dentry) -> Result<Vec<u8>, FileSystemError> {
        self.walk_path(ProjectionRoot::Filesystem(root), false)
            .map(|(path, _)| path)
    }
    fn walk_path(
        &self,
        root: ProjectionRoot<'_>,
        deleted_suffix: bool,
    ) -> Result<(Vec<u8>, bool), FileSystemError> {
        if let Some(label) = self.dentry.diagnostic_label() {
            return copy_bytes(label).map(|path| (path, false));
        }
        let mut mount = self.mount.clone();
        let mut dentry = self.dentry.clone();
        let mut components: Vec<FileName> = Vec::new();
        let mut visited = Vec::new();
        let mut deleted = false;
        let mut bytes = 0usize;
        let reached_root = loop {
            let at_root = match root {
                ProjectionRoot::Namespace(root) => {
                    let same_mount = match (&mount, &root.mount) {
                        (Some(left), Some(right)) => Arc::ptr_eq(left, right),
                        (None, None) => true,
                        _ => false,
                    };
                    same_mount && Arc::ptr_eq(&dentry, &root.dentry)
                }
                ProjectionRoot::Filesystem(root) => core::ptr::eq(dentry.as_ref(), root),
            };
            if at_root {
                break true;
            }
            let identity = (
                mount.as_ref().map_or(0, |view| Arc::as_ptr(view) as usize),
                Arc::as_ptr(&dentry) as usize,
            );
            if visited.contains(&identity) {
                return Err(FileSystemError::InvalidFileSystem);
            }
            visited
                .try_reserve(1)
                .map_err(|_| FileSystemError::OutOfMemory)?;
            visited.push(identity);
            if matches!(root, ProjectionRoot::Namespace(_))
                && let Some(view) = &mount
                && Arc::ptr_eq(&view.root, &dentry)
            {
                let Some(attachment) = view.attachment() else {
                    break false;
                };
                let Some(parent) = attachment.parent.upgrade() else {
                    break false;
                };
                mount = Some(parent);
                dentry = attachment.point;
                continue;
            }
            let Some((parent, name, unlinked)) = dentry.snapshot() else {
                return Err(FileSystemError::InvalidOperation);
            };
            deleted |= unlinked;
            let Some(parent) = parent else {
                break false;
            };
            bytes = bytes
                .checked_add(name.bytes().len() + 1)
                .ok_or(FileSystemError::NameTooLong)?;
            if bytes >= 4096 {
                return Err(FileSystemError::NameTooLong);
            }
            components
                .try_reserve(1)
                .map_err(|_| FileSystemError::OutOfMemory)?;
            components.push(name);
            dentry = parent;
        };
        deleted |= dentry.is_unlinked();
        if deleted && !deleted_suffix {
            return Err(FileSystemError::NotFound);
        }
        let unreachable = !reached_root && !deleted_suffix;
        let prefix = if unreachable {
            &b"(unreachable)"[..]
        } else {
            &b""[..]
        };
        let suffix = if deleted {
            &b" (deleted)"[..]
        } else {
            &b""[..]
        };
        let capacity = bytes
            .max(1)
            .checked_add(prefix.len())
            .and_then(|n| n.checked_add(suffix.len()))
            .ok_or(FileSystemError::NameTooLong)?;
        if capacity >= 4096 {
            return Err(FileSystemError::NameTooLong);
        }
        let mut output = Vec::new();
        output
            .try_reserve_exact(capacity)
            .map_err(|_| FileSystemError::OutOfMemory)?;
        output.extend_from_slice(prefix);
        output.push(b'/');
        for component in components.iter().rev() {
            if output.len() > prefix.len() + 1 {
                output.push(b'/');
            }
            output.extend_from_slice(component.bytes());
        }
        output.extend_from_slice(suffix);
        Ok((output, reached_root))
    }
}
fn copy_bytes(bytes: &[u8]) -> Result<Vec<u8>, FileSystemError> {
    let mut output = Vec::new();
    output
        .try_reserve_exact(bytes.len())
        .map_err(|_| FileSystemError::OutOfMemory)?;
    output.extend_from_slice(bytes);
    Ok(output)
}

impl Drop for OpenedFile {
    fn drop(&mut self) {
        if self.clear_expiry
            && let Some(view) = &self.mount
        {
            view.state.lock().expiry_mark = false;
        }
    }
}

/// 实际内容写访问：先归还 inode/可能最后 inode Drop，再归还 filesystem/view writer lease。
pub(crate) struct ContentWrite {
    // OWNER: 唯一 inode access capability；fput 可先 take 归还它，mount writer 必须
    // 保留到最后 path/inode 回收之后。None 只表示该 capability 已实际消费，无镜像 flag。
    _inode: Option<super::super::InodeAccessLease>,
    _mount: WriteOpen,
}
impl ContentWrite {
    /// 在实际 file object 的 final fput 中先归还 inode access，保留 mount writer。
    ///
    /// # Returns
    ///
    /// capability 恰好消费一次；后续 Drop 仅归还仍拥有的实际 lease。
    pub(crate) fn release_inode_access(&mut self) {
        drop(self._inode.take());
    }
}

/// Borrowed actual path consequence；只有实际 I/O producer 准入后创建，所有提前返回都执行一次触碰。
pub(crate) struct ReadAccess<'a>(&'a OpenedFile);
impl Drop for ReadAccess<'_> {
    fn drop(&mut self) {
        self.0.touch_access();
    }
}
