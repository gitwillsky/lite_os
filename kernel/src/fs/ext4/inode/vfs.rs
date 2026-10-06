use super::super::inode::Timestamp;
use super::*;

impl Inode for Ext4Inode {
    fn filesystem_id(&self) -> usize {
        Arc::as_ptr(&self.fs) as usize
    }

    fn metadata(&self) -> Result<InodeMetadata, FileSystemError> {
        let inode = self.disk.lock();
        Ok(InodeMetadata {
            filesystem: 1,
            inode: self.inode_num as u64,
            kind: inode_kind::from_mode(inode.i_mode),
            mode: inode.i_mode as u32,
            links: inode.i_links_count as u32,
            uid: inode.uid(),
            gid: inode.gid(),
            size: inode.size(),
            blocks: inode.sectors(),
            block_size: self.fs.block_size as u32,
            atime: inode.atime().seconds(),
            mtime: inode.mtime().seconds(),
            ctime: inode.ctime().seconds(),
            device: None,
        })
    }

    fn inode_type(&self) -> InodeType {
        let ino = self.disk.lock();
        inode_kind::from_mode(ino.i_mode)
    }

    fn size(&self) -> u64 {
        self.disk.lock().size()
    }

    fn is_executable(&self) -> bool {
        let ino = self.disk.lock();
        ino.i_mode & 0o111 != 0
    }

    fn read_storage(&self, offset: u64, buf: &mut [u8]) -> Result<usize, FileSystemError> {
        let mut done = 0usize;
        let size = usize::try_from(self.disk.lock().size())
            .map_err(|_| FileSystemError::InvalidOperation)?;
        let offset = usize::try_from(offset).map_err(|_| FileSystemError::InvalidOperation)?;
        if offset >= size || buf.is_empty() {
            return Ok(0);
        }
        let to_read = cmp::min(buf.len(), size - offset);
        let bs = self.fs.block_size;
        let mut cur_off = offset;
        while done < to_read {
            let blk_index = (cur_off / bs) as u32;
            let blk_off = cur_off % bs;
            let blk = self.map_block_sparse(blk_index)?;
            let n = cmp::min(bs - blk_off, to_read - done);
            let Some(blk) = blk else {
                // hole 与 unwritten extent 都读为零。
                buf[done..done + n].fill(0);
                done += n;
                cur_off += n;
                continue;
            };
            if blk_off == 0 && n == bs {
                // 完整对齐块直接读入 caller，避免 page-cache miss 为每个块分配并复制 Vec。
                self.fs.read_fs_block(blk, &mut buf[done..done + n])?;
            } else {
                // Read from actual block
                let mut b = try_zeroed(bs)?;
                self.fs.read_fs_block(blk, &mut b)?;
                buf[done..done + n].copy_from_slice(&b[blk_off..blk_off + n]);
            }
            done += n;
            cur_off += n;
        }
        // 1. Linux relatime avoids a journal transaction on every page-cache miss.
        let now = Timestamp::now();
        let inode = self.disk.lock();
        let atime = inode.atime().seconds();
        let update_atime = atime <= inode.mtime().seconds()
            || atime <= inode.ctime().seconds()
            || now.seconds() >= atime.saturating_add(86_400);
        drop(inode);
        // 2. max prevents the lock-free precheck from rolling back a concurrent explicit update.
        if update_atime {
            let mut mutation = self.fs.begin_mutation()?;
            let mut inode = mutation.inode(self)?;
            if now.seconds() > inode.atime().seconds() {
                inode.set_atime(now);
            }
            self.fs.write_inode_disk(self.inode_num, &inode)?;
            drop(inode);
            mutation.commit()?;
        }
        Ok(done)
    }

    fn read_link(&self) -> Result<Vec<u8>, FileSystemError> {
        let inode = *self.disk.lock();
        if inode_kind::from_mode(inode.i_mode) != InodeType::SymLink {
            return Err(FileSystemError::InvalidOperation);
        }
        let size = usize::try_from(inode.size()).map_err(|_| FileSystemError::InvalidFileSystem)?;
        let mut target = Vec::new();
        target
            .try_reserve_exact(size)
            .map_err(|_| FileSystemError::OutOfMemory)?;
        target.resize(size, 0);
        if Self::is_fast_symlink(&inode) {
            if !inode.copy_inline_symlink(&mut target) {
                return Err(FileSystemError::InvalidFileSystem);
            }
        } else if self.read_storage(0, &mut target)? != size {
            return Err(FileSystemError::IoError);
        }
        Ok(target)
    }

    fn write_storage(&self, offset: u64, buf: &[u8]) -> Result<usize, FileSystemError> {
        self.write_bytes(offset, buf)
    }

    fn write_storage_batch(
        &self,
        batch: &mut dyn FnMut(&mut dyn StorageWriter) -> Result<(), FileSystemError>,
    ) -> Result<(), FileSystemError> {
        self.write_batch(batch)
    }

    fn try_write_storage_batch(
        &self,
        batch: &mut dyn FnMut(&mut dyn StorageWriter) -> Result<(), FileSystemError>,
    ) -> Result<(), FileSystemError> {
        self.try_write_batch(batch)
    }

    fn append_storage(&self, buf: &[u8]) -> Result<(u64, usize), FileSystemError> {
        self.append_bytes(buf)
    }

    fn truncate_storage(&self, size: u64) -> Result<(), FileSystemError> {
        let mut mutation = self.fs.begin_mutation()?;
        self.truncate_locked(&mut mutation, size)?;
        mutation.commit()
    }

    fn allocate_storage(&self, offset: u64, length: u64) -> Result<(), FileSystemError> {
        self.allocate_range(offset, length)
    }

    fn sync_storage(&self) -> Result<(), FileSystemError> {
        self.fs.sync_journal()
    }

    fn set_times(&self, atime: Option<u64>, mtime: Option<u64>) -> Result<(), FileSystemError> {
        self.update_times(atime, mtime)
    }

    fn read_directory(
        &self,
        cursor: u64,
        visitor: &mut dyn DirectoryVisitor,
    ) -> Result<DirectoryRead, FileSystemError> {
        if self.inode_type() != InodeType::Directory {
            return Err(FileSystemError::NotDirectory);
        }
        let mut visit = |next_cursor: u64, inode: u32, file_type: u8, name: &[u8]| {
            visitor.visit(
                next_cursor,
                DirectoryEntry {
                    inode: u64::from(inode),
                    kind: inode_kind::from_file_type(file_type),
                    name,
                },
            )
        };
        if self.is_indexed() {
            self.dx_read_directory(cursor, &mut visit)
        } else {
            self.dir_iterate_from(cursor, &mut visit)
        }
    }

    fn find_child(&self, name: &[u8]) -> Result<Arc<dyn Inode>, FileSystemError> {
        if !matches!(self.inode_type(), InodeType::Directory) {
            return Err(FileSystemError::NotDirectory);
        }
        match self.lookup_entry(name)? {
            Some((number, _)) => {
                Ext4Inode::load(self.fs.clone(), number).map(|inode| inode as Arc<dyn Inode>)
            }
            None => Err(FileSystemError::NotFound),
        }
    }

    fn create(
        &self,
        name: &[u8],
        kind: InodeType,
        metadata: crate::fs::CreateMetadata,
    ) -> Result<Arc<dyn Inode>, FileSystemError> {
        if self.inode_type() != InodeType::Directory {
            return Err(FileSystemError::NotDirectory);
        }
        Self::validate_name(name)?;
        if !matches!(
            kind,
            InodeType::File | InodeType::Directory | InodeType::Socket
        ) {
            return Err(FileSystemError::InvalidOperation);
        }
        let mut mutation = self.fs.begin_mutation()?;
        if self.lookup_entry(name)?.is_some() {
            return Err(FileSystemError::AlreadyExists);
        }
        let directory = kind == InodeType::Directory;
        let parent_links = if directory {
            Some(
                link_count::increment_directory(self.disk.lock().i_links_count)
                    .map_err(link_count_error)?,
            )
        } else {
            None
        };
        let group = self.fs.group_index_and_local_inode(self.inode_num)?.0;
        let number = self.fs.allocate_inode(group, directory)?;
        mutation.discard_inode_on_abort(number)?;
        let disk = Self::new_disk(
            &self.fs,
            inode_kind::create_mode(kind, metadata.mode),
            metadata.uid,
            metadata.gid,
            if directory { 2 } else { 1 },
            kind != InodeType::Socket,
        );
        self.fs.write_inode_disk(number, &disk)?;
        let child = Ext4Inode::load(self.fs.clone(), number)?;
        if directory {
            child.initialize_directory_locked(&mut mutation, self.inode_num)?;
        }
        self.add_dir_entry_locked(&mut mutation, number, name, kind)?;
        self.touch_parent(&mut mutation, parent_links)?;
        mutation.commit()?;
        Ok(child as Arc<dyn Inode>)
    }

    fn change_owner_mode(&self, change: OwnerModeChange) -> Result<(), FileSystemError> {
        self.update_owner_mode(change)
    }

    fn symlink(
        &self,
        name: &[u8],
        target: &[u8],
        metadata: crate::fs::CreateMetadata,
    ) -> Result<Arc<dyn Inode>, FileSystemError> {
        self.create_symlink(name, target, metadata)
            .map(|inode| inode as Arc<dyn Inode>)
    }

    fn link(&self, name: &[u8], target: Arc<dyn Inode>) -> Result<(), FileSystemError> {
        self.create_hard_link(name, target)
    }

    fn unlink(&self, name: &[u8], remove_directory: bool) -> Result<(), FileSystemError> {
        if self.inode_type() != InodeType::Directory {
            return Err(FileSystemError::NotDirectory);
        }
        Self::validate_name(name)?;
        let mut mutation = self.fs.begin_mutation()?;
        let child = self.find_child(name)?;
        let metadata = child.metadata()?;
        if metadata.kind == InodeType::Directory {
            if !remove_directory {
                return Err(FileSystemError::IsDirectory);
            }
            if directory_not_empty(child.as_ref())? {
                return Err(FileSystemError::DirectoryNotEmpty);
            }
        } else if remove_directory {
            return Err(FileSystemError::NotDirectory);
        }
        let parent_links = if metadata.kind == InodeType::Directory {
            Some(
                link_count::decrement_directory(self.disk.lock().i_links_count)
                    .map_err(link_count_error)?,
            )
        } else {
            None
        };
        self.remove_dir_entry_locked(&mut mutation, name)?;
        let (child, externally_held) = self.reload_after_lookup(child, metadata.inode as u32)?;
        let mut disk = mutation.inode(&child)?;
        if metadata.kind != InodeType::Directory && disk.i_links_count > 1 {
            disk.i_links_count =
                link_count::decrement_file(disk.i_links_count).map_err(link_count_error)?;
            disk.set_ctime(Timestamp::now());
            self.fs.write_inode_disk(child.inode_num, &disk)?;
            drop(disk);
        } else if metadata.kind != InodeType::Directory && externally_held {
            drop(disk);
            self.fs.defer_reclaim_locked(&mut mutation, &child)?;
        } else {
            drop(disk);
            child.reclaim_locked(&mut mutation, metadata.kind == InodeType::Directory)?;
        }
        self.touch_parent(&mut mutation, parent_links)?;
        mutation.commit()
    }

    fn rename(
        &self,
        old_name: &[u8],
        new_parent_inode: u64,
        new_name: &[u8],
        no_replace: bool,
    ) -> Result<(), FileSystemError> {
        self.rename_entry(old_name, new_parent_inode, new_name, no_replace)
    }
}

/// 判断 inode 是否是仍待最终回收的 open-unlinked regular file 或 symlink。
///
/// `reclaim_locked` 按 Linux `ext4_free_inode` 保留 mode 并写入非零 `i_dtime`；缺少 dtime 条件时，
/// unlink 或 mount orphan recovery 已回收的 inode 会在最后一个 Arc drop 时再次进入回收，并因
/// orphan file 中已无该 inode 而以 `InvalidFileSystem` 失败。
fn awaits_orphan_reclaim(disk: &Ext4InodeDisk) -> bool {
    disk.i_links_count == 0 && disk.i_dtime == 0 && matches!(disk.i_mode & 0xF000, 0x8000 | 0xA000)
}

impl Drop for Ext4Inode {
    fn drop(&mut self) {
        let reclaim = awaits_orphan_reclaim(&self.disk.lock());
        if reclaim {
            test_orphan_drop_admission(self.inode_num);
            let result = self.reclaim_dropped_orphan();
            if let Err(error) = result {
                error!(
                    "failed to reclaim unlinked inode {}: {:?}",
                    self.inode_num, error
                );
            }
        }
    }
}

impl Ext4Inode {
    fn reclaim_dropped_orphan(&self) -> Result<(), FileSystemError> {
        let mut mutation = match MutationGuard::try_begin(&self.fs) {
            Ok(Some(mutation)) => mutation,
            Ok(None) => {
                self.fs
                    .pending_orphan_reclaim
                    .store(true, Ordering::Release);
                return Ok(());
            }
            Err(error) => {
                self.fs
                    .pending_orphan_reclaim
                    .store(true, Ordering::Release);
                return Err(error);
            }
        };
        self.reclaim_dropped_orphan_locked(&mut mutation)?;
        mutation.commit()
    }

    pub(in crate::fs::ext4) fn reclaim_dropped_orphan_locked(
        &self,
        mutation: &mut MutationGuard<'_>,
    ) -> Result<(), FileSystemError> {
        // The lock-free admission above avoids a filesystem transaction for ordinary inode drops.
        // Final Arc::drop has already made the cache Weak non-upgradeable, so only the raw inode
        // image read under the unique mutation owner is authoritative here.
        let disk = self.fs.read_inode_disk(self.inode_num)?;
        if !awaits_orphan_reclaim(&disk) {
            return Ok(());
        }
        mutation.discard_inode_on_abort(self.inode_num)?;
        self.fs.remove_orphan_locked(self.inode_num)?;
        self.reclaim_locked(mutation, false)
    }
}
