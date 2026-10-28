use super::*;

/// 创建意图下由 walk 在父目录独占持有内创建出的末项，以及 filesystem 原子创建时已打开的 backend。
pub(super) type CreatedEntry = (Arc<OpenedFile>, Option<crate::fs::InodeOpen>);
/// O_CREAT 末项的创建回调：walk 已独占持有 `parent` 并确认 `name` 不存在（Linux `lookup_open`）。
pub(super) type CreateMissing<'a> =
    &'a mut dyn FnMut(Arc<OpenedFile>, &[u8]) -> Result<CreatedEntry, FileSystemError>;

/// 唯一 walk 的末项结果；Created 只在 create intent 下、由同一次父目录独占持有内的回调产生。
/// 原始路径/链接展开路径不再由创建方重新解析，不把悬空链接的 ENOENT 当作原路径不存在。
pub(super) enum PathLookup {
    Existing(Arc<OpenedFile>),
    Created(CreatedEntry),
}

impl VirtualFileSystem {
    /// 从同一 fs context snapshot 投影 cwd；保留 Linux raw getcwd 的不可达前缀。
    pub(crate) fn current_directory_path(
        &self,
        origin: LookupOrigin,
    ) -> Result<Vec<u8>, FileSystemError> {
        let (root, start) = origin.into_parts();
        if start.inode().inode_type() != InodeType::Directory {
            return Err(FileSystemError::NotDirectory);
        }
        self.namespace()?.path(&start, &root, false)
    }

    /// 启动装配的唯一初始 root origin；用户路径必须显式传 caller origin。
    pub(crate) fn initial_origin(&self) -> Result<LookupOrigin, FileSystemError> {
        let root = self.root_opened()?;
        Ok(LookupOrigin::new(root.clone(), root))
    }
    pub(crate) fn open_file(&self, path: &[u8]) -> Result<Arc<OpenedFile>, FileSystemError> {
        if path.first() != Some(&b'/') {
            return Err(FileSystemError::InvalidPath);
        }
        self.open_file_at(self.initial_origin()?, path, &AccessIdentity::root())
    }

    pub(crate) fn open_at(
        &self,
        origin: LookupOrigin,
        path: &[u8],
        identity: &AccessIdentity,
    ) -> Result<Arc<dyn Inode>, FileSystemError> {
        self.open_file_at(origin, path, identity)
            .map(|opened| opened.inode())
    }

    /// 相对 opened directory 解析 pathname 并保留最终目录项身份。
    ///
    /// # Parameters
    ///
    /// - `origin`: caller fs root 与相对 lookup 起点的同锁快照。
    /// - `path`: raw pathname。
    /// - `identity`: traversal credential snapshot。
    ///
    /// # Returns
    ///
    /// 最终 opened entry。
    ///
    /// # Errors
    ///
    /// traversal、symlink 或资源失败时返回明确错误。
    pub(crate) fn open_file_at(
        &self,
        origin: LookupOrigin,
        path: &[u8],
        identity: &AccessIdentity,
    ) -> Result<Arc<OpenedFile>, FileSystemError> {
        self.resolve_from(origin, path, false, identity)
    }

    /// 解析 pathname 但不跟随最终 symbolic link，保留最终目录项身份（`UMOUNT_NOFOLLOW`）。
    ///
    /// # Errors
    ///
    /// traversal 或资源失败时返回明确错误。
    pub(crate) fn open_file_at_no_follow(
        &self,
        origin: LookupOrigin,
        path: &[u8],
        identity: &AccessIdentity,
    ) -> Result<Arc<OpenedFile>, FileSystemError> {
        self.resolve_from(origin, path, true, identity)
    }

    pub(super) fn resolve_from(
        &self,
        origin: LookupOrigin,
        path: &[u8],
        allow_final_symlink: bool,
        identity: &AccessIdentity,
    ) -> Result<Arc<OpenedFile>, FileSystemError> {
        match self.resolve_from_with_limit(origin, path, allow_final_symlink, identity, 0, None)? {
            PathLookup::Existing(opened) => Ok(opened),
            PathLookup::Created(_) => unreachable!("lookup without create intent"),
        }
    }

    /// O_CREAT 与普通 lookup 共用同一 walk；仅最终 normal component 缺失时调用 `create`。
    ///
    /// # Parameters
    /// - `origin`：caller 的 root/start 快照，链接展开不替换 root。
    /// - `path`：原始 pathname bytes。
    /// - `keep_final_symlink`：O_EXCL/O_NOFOLLOW 保留无尾随斜杠的末项链接。
    /// - `identity`：整次 walk 与创建使用的 credential snapshot。
    /// - `create`：父目录已独占持有且末项不存在时调用的创建方。
    ///
    /// # Returns
    /// 已存在的实际路径，或同次父目录独占持有内创建的路径/backend。
    ///
    /// # Errors
    /// 传播 traversal、目录锁分配与创建方错误，不重走原 pathname。
    pub(super) fn resolve_for_create(
        &self,
        origin: LookupOrigin,
        path: &[u8],
        keep_final_symlink: bool,
        identity: &AccessIdentity,
        create: CreateMissing<'_>,
    ) -> Result<PathLookup, FileSystemError> {
        self.resolve_from_with_limit(origin, path, keep_final_symlink, identity, 0, Some(create))
    }

    fn resolve_from_with_limit(
        &self,
        origin: LookupOrigin,
        path: &[u8],
        allow_final_symlink: bool,
        identity: &AccessIdentity,
        followed_links: usize,
        mut create: Option<CreateMissing<'_>>,
    ) -> Result<PathLookup, FileSystemError> {
        const MAX_SYMLINKS: usize = 40;
        if path.is_empty() {
            return Err(FileSystemError::NotFound);
        }
        let (root, start) = origin.into_parts();
        let opened = if path.first() == Some(&b'/') {
            root.clone()
        } else {
            start
        };
        let mut opened = self.enter_mount(opened)?;
        let component_count = path
            .split(|byte| *byte == b'/')
            .filter(|component| !component.is_empty())
            .count();
        for (index, component) in path
            .split(|byte| *byte == b'/')
            .filter(|component| !component.is_empty())
            .enumerate()
        {
            if opened.inode().inode_type() != InodeType::Directory {
                return Err(FileSystemError::NotDirectory);
            }
            opened.inode().require_access(identity, 1)?;
            match component {
                b"." => {}
                b".." => {
                    if opened.same_path(&root) {
                        continue;
                    }
                    if let Some(parent) = opened.parent(&root)? {
                        opened = self.enter_mount(parent)?;
                    }
                }
                name => {
                    if name.len() > 255 {
                        return Err(FileSystemError::NameTooLong);
                    }
                    let parent = opened.clone();
                    let creating = create.is_some() && index + 1 == component_count;
                    // lookup_fast_for_open：父目录搜索成功后，O_CREAT normal 末项尾随斜杠为 EISDIR。
                    if creating && path.last() == Some(&b'/') {
                        return Err(FileSystemError::IsDirectory);
                    }
                    // Linux lookup_slow/lookup_open：find_child 与 dentry admission 在父目录锁内，
                    // 与同目录的 unlink/rename 发布互斥；O_CREAT 末项独占，查找与创建不被插入。
                    let child = {
                        let (_shared, _exclusive) = if creating {
                            (None, Some(parent.dentry.lock_exclusive()?))
                        } else {
                            (Some(parent.dentry.lock_shared()?), None)
                        };
                        // Linux IS_DEADDIR：已删除的目录里不再查找或创建。
                        if parent.is_unlinked() {
                            return Err(FileSystemError::NotFound);
                        }
                        match parent.inode().find_child(name) {
                            Ok(inode) => OpenedFile::child(inode, parent.clone(), name)?,
                            Err(FileSystemError::NotFound) if creating => {
                                let create = create.as_deref_mut().expect("create intent");
                                return create(parent.clone(), name).map(PathLookup::Created);
                            }
                            Err(error) => return Err(error),
                        }
                    };
                    opened = self.enter_mount(child)?;
                    let is_untrailed_final = index + 1 == component_count
                        && path.last().is_none_or(|byte| *byte != b'/');
                    if opened.inode().inode_type() == InodeType::SymLink
                        && !(allow_final_symlink && is_untrailed_final)
                    {
                        if followed_links >= MAX_SYMLINKS {
                            return Err(FileSystemError::Loop);
                        }
                        opened.touch_access();
                        let target = match opened.inode().link_target()? {
                            super::super::LinkTarget::Text(target) => target,
                            super::super::LinkTarget::Opened(target) => {
                                let mut remaining = Vec::new();
                                remaining
                                    .try_reserve_exact(path.len())
                                    .map_err(|_| FileSystemError::OutOfMemory)?;
                                for part in path
                                    .split(|byte| *byte == b'/')
                                    .filter(|part| !part.is_empty())
                                    .skip(index + 1)
                                {
                                    if !remaining.is_empty() {
                                        remaining.push(b'/');
                                    }
                                    remaining.extend_from_slice(part);
                                }
                                if remaining.is_empty() {
                                    if path.last() == Some(&b'/')
                                        && target.inode().inode_type() != InodeType::Directory
                                    {
                                        return Err(FileSystemError::NotDirectory);
                                    }
                                    return Ok(PathLookup::Existing(target));
                                }
                                // Magic link 后仍有组件时，trailing slash 同样要求最终为目录；
                                // remaining 是原 path 子序列，预留 path.len() 已覆盖这个 slash。
                                if path.last() == Some(&b'/') {
                                    remaining.push(b'/');
                                }
                                return self.resolve_from_with_limit(
                                    LookupOrigin::new(root, target),
                                    &remaining,
                                    allow_final_symlink,
                                    identity,
                                    followed_links + 1,
                                    create,
                                );
                            }
                        };
                        if target.is_empty() {
                            return Err(FileSystemError::NotFound);
                        }
                        let remaining = path
                            .split(|byte| *byte == b'/')
                            .filter(|part| !part.is_empty())
                            .skip(index + 1);
                        let mut expanded = target;
                        // remaining path 是原 path 的子序列；一次预留 path.len()
                        // 覆盖所有分隔符与 trailing slash，缺失时 push 会走全局 OOM abort。
                        expanded
                            .try_reserve(path.len())
                            .map_err(|_| FileSystemError::OutOfMemory)?;
                        for part in remaining {
                            if expanded.last() != Some(&b'/') {
                                expanded.push(b'/');
                            }
                            expanded.extend_from_slice(part);
                        }
                        if path.last() == Some(&b'/') && expanded.last() != Some(&b'/') {
                            expanded.push(b'/');
                        }
                        return self.resolve_from_with_limit(
                            LookupOrigin::new(root, parent),
                            &expanded,
                            allow_final_symlink,
                            identity,
                            followed_links + 1,
                            create,
                        );
                    }
                }
            }
        }
        // 只有实际消费 component 才进行父目录 search；纯 `/` 不额外要求最终 inode X。
        // O_PATH/final directory read 由各 open capability 授权，不能把 X 当成 universal final check。
        if path.len() > 1
            && path.last() == Some(&b'/')
            && opened.inode().inode_type() != InodeType::Directory
        {
            return Err(FileSystemError::NotDirectory);
        }
        Ok(PathLookup::Existing(opened))
    }

    pub(super) fn parent_from(
        &self,
        origin: LookupOrigin,
        path: &[u8],
        identity: &AccessIdentity,
    ) -> Result<(Arc<OpenedFile>, Vec<u8>), FileSystemError> {
        let trimmed = path.strip_suffix(b"/").unwrap_or(path);
        let split = trimmed.iter().rposition(|byte| *byte == b'/');
        let (parent_path, name) = match split {
            Some(0) => (&b"/"[..], &trimmed[1..]),
            Some(index) => (&trimmed[..index], &trimmed[index + 1..]),
            None => (&b"."[..], trimmed),
        };
        if name.is_empty() {
            return Err(FileSystemError::InvalidPath);
        }
        let parent = self.resolve_from(origin, parent_path, false, identity)?;
        if name.len() > 255 {
            return Err(FileSystemError::NameTooLong);
        }
        let mut owned_name = Vec::new();
        owned_name
            .try_reserve_exact(name.len())
            .map_err(|_| FileSystemError::OutOfMemory)?;
        owned_name.extend_from_slice(name);
        Ok((parent, owned_name))
    }
}
