use super::super::inode::Timestamp;
use super::*;

impl Ext4Inode {
    /// ext4 inode 号。
    pub(crate) fn number(&self) -> u64 {
        u64::from(self.inode_num)
    }

    /// 是否为根目录。
    pub(crate) fn is_root(&self) -> bool {
        self.inode_num == EXT4_ROOT_INO
    }

    pub(crate) fn read_xattr(&self, name: &[u8]) -> Result<Vec<u8>, FileSystemError> {
        self.read_attribute(name)
    }

    /// 列出 xattr 名称；`trusted` 决定是否包含 `trusted.*`（调用者具有 `CAP_SYS_ADMIN`）。
    pub(crate) fn list_xattrs(&self, trusted: bool) -> Result<Vec<u8>, FileSystemError> {
        self.list_attributes(trusted)
    }

    pub(crate) fn change_xattr(
        &self,
        change: crate::vfs::XattrChange<'_>,
    ) -> Result<(), FileSystemError> {
        self.change_attribute(change)
    }

    pub(crate) fn metadata(&self) -> Result<InodeMetadata, FileSystemError> {
        let inode = self.disk.lock();
        Ok(InodeMetadata {
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
            fine_times: crate::vfs::FineTimes {
                atime_nsec: inode.atime().nanoseconds(),
                mtime_nsec: inode.mtime().nanoseconds(),
                ctime_nsec: inode.ctime().nanoseconds(),
                birth: inode
                    .birth_time()
                    .map(|time| (time.seconds(), time.nanoseconds())),
            },
            device: Self::rdev(&inode),
        })
    }

    pub(crate) fn inode_type(&self) -> InodeType {
        let ino = self.disk.lock();
        inode_kind::from_mode(ino.i_mode)
    }

    pub(crate) fn size(&self) -> u64 {
        self.disk.lock().size()
    }

    pub(crate) fn read_storage(
        &self,
        offset: u64,
        buf: &mut [u8],
    ) -> Result<usize, FileSystemError> {
        let size = usize::try_from(self.disk.lock().size())
            .map_err(|_| FileSystemError::InvalidOperation)?;
        let offset = usize::try_from(offset).map_err(|_| FileSystemError::InvalidOperation)?;
        if offset >= size || buf.is_empty() {
            return Ok(0);
        }
        let to_read = cmp::min(buf.len(), size - offset);
        let bs = self.fs.block_size;
        // 1. 遍历 extent：hole 与 unwritten 立即补零，同一 extent 内连续的完整对齐块记为一段，
        //    首尾不对齐的块同步读取后截取。
        // 2. 收集到的各段（可能分属不同 extent）作为一批提交，设备并发处理，
        //    避免碎片化文件的 extent 间逐段往返。
        let mut rest: &mut [u8] = &mut buf[..to_read];
        let mut runs: Vec<(u64, &mut [u8])> = Vec::new();
        let mut cur_off = offset;
        while !rest.is_empty() {
            let blk_index = (cur_off / bs) as u32;
            let blk_off = cur_off % bs;
            let n = cmp::min(bs - blk_off, rest.len());
            let Some((blk, run_blocks)) = self.map_run(blk_index)? else {
                // hole 与 unwritten extent 都读为零。
                let (head, tail) = core::mem::take(&mut rest).split_at_mut(n);
                head.fill(0);
                rest = tail;
                cur_off += n;
                continue;
            };
            if blk_off == 0 && n == bs {
                let blocks = cmp::min(run_blocks, (rest.len() / bs) as u64) as usize;
                let (head, tail) = core::mem::take(&mut rest).split_at_mut(blocks * bs);
                runs.try_reserve(1)
                    .map_err(|_| FileSystemError::OutOfMemory)?;
                runs.push((blk, head));
                rest = tail;
                cur_off += blocks * bs;
                continue;
            }
            let mut b = try_zeroed(bs)?;
            self.fs.read_fs_block(blk, &mut b)?;
            let (head, tail) = core::mem::take(&mut rest).split_at_mut(n);
            head.copy_from_slice(&b[blk_off..blk_off + n]);
            rest = tail;
            cur_off += n;
        }
        if !runs.is_empty() {
            self.fs.read_fs_runs(&mut runs)?;
        }
        Ok(to_read)
    }

    /// 读访问后按 relatime 更新 atime。
    pub(crate) fn touch_access(&self, now: u64) -> Result<(), FileSystemError> {
        let needs_update = |inode: &Ext4InodeDisk| {
            crate::vfs::needs_access_update(
                inode.atime().seconds(),
                inode.mtime().seconds(),
                inode.ctime().seconds(),
                now,
            )
        };
        if !needs_update(&self.disk.lock()) {
            return Ok(());
        }
        let mut mutation = self.fs.begin_mutation()?;
        let mut inode = mutation.inode(self)?;
        if needs_update(&inode) {
            inode.set_atime(Timestamp::from_seconds(now)?);
            self.fs.write_inode_disk(self.inode_num, &inode)?;
        }
        drop(inode);
        mutation.commit()
    }

    pub(crate) fn read_link(&self) -> Result<Vec<u8>, FileSystemError> {
        let inode = self.disk.lock().disk;
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

    pub(crate) fn write_storage(
        &self,
        offset: u64,
        buf: &[u8],
        policy: Option<crate::vfs::WritePolicy>,
    ) -> Result<usize, FileSystemError> {
        self.write_bytes(offset, buf, policy)
    }

    /// 写回缓存模式下的写入：数据经页缓存写回，set-ID 清除在写入开始时单独提交。
    pub(crate) fn prepare_cached_write(
        &self,
        policy: crate::vfs::WritePolicy,
    ) -> Result<(), FileSystemError> {
        let mut mutation = self.fs.begin_mutation()?;
        self.remove_write_privileges_locked(&mut mutation, Some(policy))?;
        mutation.commit()
    }

    /// 为缓冲写入 `[offset, offset + length)` 预留尚未分配的 block（延迟分配），登记到 `reserved`。
    ///
    /// `reserved` 是调用者（内容缓存）按文件持有的“已预留的 file block”集合：已在集合里的块不重复预留，
    /// 回写分配它们之前由调用者归还。返回本次新加入的块，写入失败只撤销这些块，不影响已有脏数据的预留。
    ///
    /// # Errors
    /// 空闲 block 不足返回 `NoSpace`（此时 `reserved` 与文件系统预留都不变）。
    pub(crate) fn reserve_buffered_write(
        &self,
        offset: u64,
        length: usize,
        reserved: &mut alloc::collections::BTreeSet<u32>,
    ) -> Result<Vec<u32>, FileSystemError> {
        if length == 0 {
            return Ok(Vec::new());
        }
        let block_size = self.fs.block_size as u64;
        let first = u32::try_from(offset / block_size).map_err(|_| FileSystemError::NoSpace)?;
        let last = u32::try_from((offset + length as u64 - 1) / block_size)
            .map_err(|_| FileSystemError::NoSpace)?;
        let mut missing = Vec::new();
        for block in first..=last {
            if !reserved.contains(&block) && self.is_hole(block)? {
                missing
                    .try_reserve(1)
                    .map_err(|_| FileSystemError::OutOfMemory)?;
                missing.push(block);
            }
        }
        if missing.is_empty() {
            return Ok(Vec::new());
        }
        self.fs.reserve_delalloc(missing.len() as u64)?;
        reserved.extend(missing.iter().copied());
        Ok(missing)
    }

    /// 文件系统块大小（字节）。
    pub(crate) fn block_size(&self) -> usize {
        self.fs.block_size
    }

    /// 归还 `blocks` 个块的延迟分配预留（回写即将分配它们，或预留所属的区间被丢弃）。
    pub(crate) fn release_buffered_reservation(&self, blocks: u64) {
        if blocks != 0 {
            self.fs.release_delalloc(blocks);
        }
    }

    /// 回写分配失败后补回 `blocks` 个块的预留。
    pub(crate) fn restore_buffered_reservation(&self, blocks: u64) {
        if blocks != 0 {
            self.fs.restore_delalloc(blocks);
        }
    }

    /// 缓冲写入已进入内容缓存之后提交文件大小与修改时间（数据本身由回写落盘）。
    ///
    /// 只在大小增长或修改时间的秒数变化时开一次 journal mutation，连续的小写入不会每次都改 inode。
    pub(crate) fn note_buffered_write(&self, end: u64) -> Result<(), FileSystemError> {
        let now = Timestamp::now();
        {
            let inode = self.disk.lock();
            if end <= inode.size() && inode.mtime().seconds() == now.seconds() {
                return Ok(());
            }
        }
        let mut mutation = self.fs.begin_mutation()?;
        self.finish_write_locked(
            &mut mutation,
            usize::try_from(end).map_err(|_| FileSystemError::NoSpace)?,
        )?;
        mutation.commit()
    }

    pub(crate) fn truncate_storage(
        &self,
        size: u64,
        policy: Option<crate::vfs::WritePolicy>,
    ) -> Result<(), FileSystemError> {
        let mut mutation = self.fs.begin_mutation()?;
        self.truncate_locked(&mut mutation, size, policy)?;
        mutation.commit()
    }

    pub(crate) fn allocate_storage(
        &self,
        offset: u64,
        length: u64,
        policy: Option<crate::vfs::WritePolicy>,
    ) -> Result<(), FileSystemError> {
        self.allocate_range(offset, length, policy)
    }

    pub(crate) fn sync_storage(&self) -> Result<(), FileSystemError> {
        self.fs.sync_journal()
    }

    pub(crate) fn set_times(
        &self,
        atime: Option<u64>,
        mtime: Option<u64>,
    ) -> Result<(), FileSystemError> {
        self.update_times(atime, mtime)
    }

    pub(crate) fn read_directory(
        &self,
        cursor: u64,
        visitor: &mut dyn DirectoryVisitor,
    ) -> Result<DirectoryRead, FileSystemError> {
        if self.inode_type() != InodeType::Directory {
            return Err(FileSystemError::NotDirectory);
        }
        // Linux iterate_dir 在 dead directory 上返回 ENOENT；实际 zero-link fact 拥有该阶段。
        if self.disk.lock().i_links_count == 0 {
            return Err(FileSystemError::NotFound);
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

    pub(crate) fn find_child(&self, name: &[u8]) -> Result<Arc<Ext4Inode>, FileSystemError> {
        if !matches!(self.inode_type(), InodeType::Directory) {
            return Err(FileSystemError::NotDirectory);
        }
        match self.lookup_entry(name)? {
            Some((number, _)) => Ext4Inode::load(self.fs.clone(), number),
            None => Err(FileSystemError::NotFound),
        }
    }

    pub(crate) fn create(
        &self,
        name: &[u8],
        kind: InodeType,
        metadata: crate::vfs::CreateMetadata,
    ) -> Result<Arc<Ext4Inode>, FileSystemError> {
        if !matches!(
            kind,
            InodeType::File | InodeType::Directory | InodeType::Socket
        ) {
            return Err(FileSystemError::InvalidOperation);
        }
        self.create_node(name, kind, metadata, None)
    }

    pub(crate) fn mknod(
        &self,
        name: &[u8],
        kind: InodeType,
        metadata: crate::vfs::CreateMetadata,
        device: Option<crate::vfs::DeviceNumber>,
    ) -> Result<Arc<Ext4Inode>, FileSystemError> {
        match (kind, device) {
            (InodeType::Fifo, None)
            | (InodeType::CharacterDevice | InodeType::BlockDevice, Some(_)) => {}
            _ => return Err(FileSystemError::InvalidOperation),
        }
        self.create_node(name, kind, metadata, device)
    }

    pub(crate) fn change_owner_mode(&self, change: OwnerModeChange) -> Result<(), FileSystemError> {
        self.update_owner_mode(change)
    }

    pub(crate) fn symlink(
        &self,
        name: &[u8],
        target: &[u8],
        metadata: crate::vfs::CreateMetadata,
    ) -> Result<Arc<Ext4Inode>, FileSystemError> {
        self.create_symlink(name, target, metadata)
    }

    pub(crate) fn link(&self, name: &[u8], target: Arc<Ext4Inode>) -> Result<(), FileSystemError> {
        self.create_hard_link(name, target)
    }

    pub(crate) fn unlink(
        &self,
        name: &[u8],
        remove_directory: bool,
    ) -> Result<(), FileSystemError> {
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
            if directory_not_empty(&child)? {
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
        } else if externally_held {
            drop(disk);
            self.fs.defer_reclaim_locked(&mut mutation, &child)?;
        } else {
            drop(disk);
            child.reclaim_locked(&mut mutation, metadata.kind == InodeType::Directory)?;
        }
        self.touch_parent(&mut mutation, parent_links)?;
        mutation.commit()
    }

    pub(crate) fn rename(
        &self,
        old_name: &[u8],
        new_parent_inode: u64,
        new_name: &[u8],
        no_replace: bool,
    ) -> Result<(), FileSystemError> {
        self.rename_entry(old_name, new_parent_inode, new_name, no_replace)
    }
}

/// 判断所有 inode 类型是否仍等待 open-unlinked 最终回收。
///
/// `reclaim_locked` 按 Linux `ext4_free_inode` 保留 mode 并写入非零 `i_dtime`；缺少 dtime 条件时，
/// unlink 或 mount orphan recovery 已回收的 inode 会在最后一个 Arc drop 时再次进入回收，并因
/// orphan file 中已无该 inode 而以 `InvalidFileSystem` 失败。
fn awaits_orphan_reclaim(disk: &Ext4InodeDisk) -> bool {
    // Linux ext4_evict_inode 对所有零链接 inode 回收；目录的 open/cwd 引用也保留 inode number。
    disk.i_links_count == 0 && disk.i_dtime == 0
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

    pub(in crate::ext4) fn reclaim_dropped_orphan_locked(
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
        self.reclaim_locked(
            mutation,
            inode_kind::from_mode(disk.i_mode) == InodeType::Directory,
        )
    }
}

impl Ext4Inode {
    /// 在 `self` 目录下创建一个新 inode 的唯一实现（Linux `ext4_create`/`ext4_mknod` 共用路径）。
    ///
    /// # Parameters
    ///
    /// - `kind`: 调用者已校验过的 inode 类型。
    /// - `device`: 设备节点的设备号，写入 `i_block`；其余类型为 `None`。
    ///
    /// # Errors
    ///
    /// 非目录、名字无效、已存在、inode 分配或 journal 失败时返回对应错误。
    fn create_node(
        &self,
        name: &[u8],
        kind: InodeType,
        metadata: crate::vfs::CreateMetadata,
        device: Option<crate::vfs::DeviceNumber>,
    ) -> Result<Arc<Ext4Inode>, FileSystemError> {
        if self.inode_type() != InodeType::Directory {
            return Err(FileSystemError::NotDirectory);
        }
        Self::validate_name(name)?;
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
        let mut disk = Self::new_disk(
            &self.fs,
            inode_kind::create_mode(kind, metadata.mode),
            metadata.uid,
            metadata.gid,
            if directory { 2 } else { 1 },
            // 没有数据块的节点（socket、FIFO、设备）不带 extent tree。
            kind == InodeType::File || kind == InodeType::Directory,
        );
        if let Some(device) = device {
            disk.set_block_bytes(&inode_kind::encode_device(device.major, device.minor));
        }
        self.fs.write_inode_disk(number, &disk)?;
        let child = Ext4Inode::load(self.fs.clone(), number)?;
        if directory {
            child.initialize_directory_locked(&mut mutation, self.inode_num)?;
        }
        self.add_dir_entry_locked(&mut mutation, number, name, kind)?;
        self.touch_parent(&mut mutation, parent_links)?;
        mutation.commit()?;
        Ok(child as Arc<Ext4Inode>)
    }

    /// 设备节点的设备号；非设备 inode 为 `None`。
    pub(in crate::ext4) fn rdev(disk: &Ext4InodeDisk) -> Option<crate::vfs::DeviceNumber> {
        matches!(disk.i_mode & 0xF000, 0x2000 | 0x6000).then(|| {
            let (major, minor) = inode_kind::decode_device(&disk.block_bytes());
            crate::vfs::DeviceNumber::new(major, minor)
        })
    }
}
