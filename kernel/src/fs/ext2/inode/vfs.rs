use super::*;

impl Inode for Ext2Inode {
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
            size: Self::disk_size(&inode),
            blocks: inode.i_blocks_lo as u64,
            block_size: self.fs.block_size as u32,
            atime: inode.i_atime as u64,
            mtime: inode.i_mtime as u64,
            ctime: inode.i_ctime as u64,
            device: None,
        })
    }

    fn inode_type(&self) -> InodeType {
        let ino = self.disk.lock();
        inode_kind::from_mode(ino.i_mode)
    }

    fn size(&self) -> u64 {
        let ino = self.disk.lock();
        Self::disk_size(&ino)
    }

    fn is_executable(&self) -> bool {
        let ino = self.disk.lock();
        ino.i_mode & 0o111 != 0
    }

    fn read_storage(&self, offset: u64, buf: &mut [u8]) -> Result<usize, FileSystemError> {
        let mut done = 0usize;
        let ino = self.disk.lock();
        let size = usize::try_from(Self::disk_size(&ino))
            .map_err(|_| FileSystemError::InvalidOperation)?;
        drop(ino);
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
            if blk == 0 {
                // This is a hole - fill with zeros
                buf[done..done + n].fill(0);
            } else if blk_off == 0 && n == bs {
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
        let now = Self::now();
        let inode = self.disk.lock();
        let atime = inode.i_atime;
        let update_atime =
            atime <= inode.i_mtime || atime <= inode.i_ctime || now >= atime.saturating_add(86_400);
        drop(inode);
        // 2. max prevents the lock-free precheck from rolling back a concurrent explicit update.
        if update_atime {
            let mut mutation = self.fs.begin_mutation()?;
            let mut inode = mutation.inode(self)?;
            inode.i_atime = cmp::max(inode.i_atime, now);
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
        let size = usize::try_from(Self::disk_size(&inode))
            .map_err(|_| FileSystemError::InvalidFileSystem)?;
        let mut target = Vec::new();
        target
            .try_reserve_exact(size)
            .map_err(|_| FileSystemError::OutOfMemory)?;
        target.resize(size, 0);
        if size <= core::mem::size_of::<[u32; 15]>() {
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
        self.fs.device.flush().map_err(block_error)
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
        self.dir_iterate_from(cursor, |next_cursor, header, name| {
            if header.inode == 0 {
                return Ok(DirectoryVisit::Continue);
            }
            let kind = match header.file_type {
                2 => InodeType::Directory,
                7 => InodeType::SymLink,
                3 => InodeType::CharacterDevice,
                5 => InodeType::Fifo,
                6 => InodeType::Socket,
                _ => InodeType::File,
            };
            visitor.visit(
                next_cursor,
                DirectoryEntry {
                    inode: header.inode as u64,
                    kind,
                    name,
                },
            )
        })
    }

    fn find_child(&self, name: &[u8]) -> Result<Arc<dyn Inode>, FileSystemError> {
        if !matches!(self.inode_type(), InodeType::Directory) {
            return Err(FileSystemError::NotDirectory);
        }
        let mut found: Option<u32> = None;
        self.dir_iterate_from(0, |_next_cursor, hdr, name_bytes| {
            if hdr.inode != 0 && name_bytes == name {
                found = Some(hdr.inode);
                return Ok(DirectoryVisit::Stop);
            }
            Ok(DirectoryVisit::Continue)
        })?;
        if let Some(ino) = found {
            return Ext2Inode::load(self.fs.clone(), ino).map(|x| x as Arc<dyn Inode>);
        }
        Err(FileSystemError::NotFound)
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
        match self.find_child(name) {
            Ok(_) => return Err(FileSystemError::AlreadyExists),
            Err(FileSystemError::NotFound) => {}
            Err(error) => return Err(error),
        }
        let parent_links = if kind == InodeType::Directory {
            Some(link_count::increment(self.disk.lock().i_links_count).map_err(link_count_error)?)
        } else {
            None
        };
        let group = self.fs.group_index_and_local_inode(self.inode_num).0;
        let number = self
            .fs
            .allocate_inode(group, kind == InodeType::Directory)?;
        mutation.discard_inode_on_abort(number)?;
        let now = Self::now();
        let mut disk = Ext2InodeDisk {
            i_mode: inode_kind::create_mode(kind, metadata.mode),
            i_atime: now,
            i_ctime: now,
            i_mtime: now,
            i_links_count: if kind == InodeType::Directory { 2 } else { 1 },
            ..Default::default()
        };
        disk.set_uid(metadata.uid);
        disk.set_gid(metadata.gid);
        self.fs.write_inode_disk(number, &disk)?;
        let child = Ext2Inode::load(self.fs.clone(), number)?;
        if kind == InodeType::Directory {
            child.add_dir_entry_locked(&mut mutation, number, b".", InodeType::Directory)?;
            child.add_dir_entry_locked(
                &mut mutation,
                self.inode_num,
                b"..",
                InodeType::Directory,
            )?;
        }
        self.add_dir_entry_locked(&mut mutation, number, name, kind)?;
        let mut parent = mutation.inode(self)?;
        if let Some(parent_links) = parent_links {
            parent.i_links_count = parent_links;
        }
        parent.i_mtime = now;
        parent.i_ctime = now;
        self.fs.write_inode_disk(self.inode_num, &parent)?;
        drop(parent);
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
            Some(link_count::decrement(self.disk.lock().i_links_count).map_err(link_count_error)?)
        } else {
            None
        };
        self.remove_dir_entry_locked(&mut mutation, name)?;
        let (child, externally_held) = self.reload_after_lookup(child, metadata.inode as u32)?;
        let mut disk = mutation.inode(&child)?;
        if metadata.kind != InodeType::Directory && disk.i_links_count > 1 {
            disk.i_links_count =
                link_count::decrement(disk.i_links_count).map_err(link_count_error)?;
            disk.i_ctime = Self::now();
            self.fs.write_inode_disk(child.inode_num, &disk)?;
            drop(disk);
        } else if metadata.kind != InodeType::Directory && externally_held {
            drop(disk);
            self.fs.defer_reclaim_locked(&mut mutation, &child)?;
        } else {
            drop(disk);
            child.reclaim_locked(&mut mutation, metadata.kind == InodeType::Directory)?;
        }
        let mut parent = mutation.inode(self)?;
        if let Some(parent_links) = parent_links {
            parent.i_links_count = parent_links;
        }
        parent.i_mtime = Self::now();
        parent.i_ctime = parent.i_mtime;
        self.fs.write_inode_disk(self.inode_num, &parent)?;
        drop(parent);
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

impl Drop for Ext2Inode {
    fn drop(&mut self) {
        let reclaim = {
            let disk = self.disk.lock();
            disk.i_links_count == 0 && matches!(disk.i_mode & 0xF000, 0x8000 | 0xA000)
        };
        if reclaim {
            test_orphan_drop_admission(self.inode_num);
            let result = self.reclaim_dropped_orphan();
            if let Err(error) = result {
                error!(
                    "[EXT2] failed to reclaim unlinked inode {}: {:?}",
                    self.inode_num, error
                );
            }
        }
    }
}

impl Ext2Inode {
    fn reclaim_dropped_orphan(&self) -> Result<(), FileSystemError> {
        let mut mutation = self.fs.begin_mutation()?;
        // The lock-free admission above avoids a filesystem transaction for ordinary inode drops.
        // `i_dtime` is chain topology and may be rewritten by an earlier orphan reclaim, so its
        // authoritative value must be read only after acquiring the unique mutation owner.
        // Final Arc::drop has already made the cache Weak non-upgradeable. A predecessor reclaim
        // may therefore have updated the on-disk chain through another temporary identity; only
        // the raw inode image under the mutation owner is authoritative here.
        let disk = self.fs.read_inode_disk(self.inode_num)?;
        if disk.i_links_count != 0 || !matches!(disk.i_mode & 0xF000, 0x8000 | 0xA000) {
            return Ok(());
        }
        let orphan_next = disk.i_dtime;
        mutation.discard_inode_on_abort(self.inode_num)?;
        self.fs
            .remove_orphan_locked(&mut mutation, self.inode_num, orphan_next)?;
        self.reclaim_locked(&mut mutation, false)?;
        mutation.commit()
    }
}
