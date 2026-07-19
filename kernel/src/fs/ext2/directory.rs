use super::*;

impl Ext2Inode {
    /// @description 释放 namespace lookup Arc，重新取得 concrete inode 并冻结真实 external ownership。
    /// @return concrete inode 与是否存在除本地 owner 之外的 live Arc。
    /// @errors inode reload 的 filesystem、I/O 或 allocation 错误。
    pub(super) fn reload_after_lookup(
        &self,
        lookup: Arc<dyn Inode>,
        number: u32,
    ) -> Result<(Arc<Self>, bool), FileSystemError> {
        drop(lookup);
        let inode = Self::load(self.fs.clone(), number)?;
        let externally_held = Arc::strong_count(&inode) > 1;
        Ok((inode, externally_held))
    }

    /// @description 在 active mutation 中向目录块插入唯一 ext2 dir entry。
    /// @param mutation 为目录 inode 扩容时先捕获 live disk preimage 的 transaction owner。
    /// @return entry 与可能增长的 directory size 已 staged。
    /// @errors 块分配、layout、journal、I/O 或 rollback reserve 错误。
    pub(super) fn add_dir_entry_locked(
        &self,
        mutation: &mut MutationGuard<'_>,
        child: u32,
        name: &[u8],
        kind: InodeType,
    ) -> Result<(), FileSystemError> {
        let needed = align_up(Ext2DirEntry2Header::SIZE + name.len(), 4);
        let blocks = ceil_div(self.size() as usize, self.fs.block_size);
        for index in 0..=blocks {
            let block = if index == blocks {
                self.ensure_block_mapped(mutation, index as u32)?
            } else {
                self.map_block(index as u32)?
            };
            let mut buf = try_zeroed(self.fs.block_size)?;
            if index < blocks {
                let cached = self.fs.read_metadata_block(block)?;
                buf.copy_from_slice(&cached);
            }
            if index == blocks {
                let header = Ext2DirEntry2Header {
                    inode: child,
                    rec_len: self.fs.block_size as u16,
                    name_len: name.len() as u8,
                    file_type: inode_kind::file_type(kind),
                };
                if !header.encode(&mut buf, 0) {
                    return Err(FileSystemError::InvalidFileSystem);
                }
                buf[Ext2DirEntry2Header::SIZE..Ext2DirEntry2Header::SIZE + name.len()]
                    .copy_from_slice(name);
                self.fs.write_fs_block(block, &buf)?;
                let mut inode = mutation.inode(self)?;
                Self::set_disk_size(&mut inode, ((index + 1) * self.fs.block_size) as u64);
                self.fs.write_inode_disk(self.inode_num, &inode)?;
                return Ok(());
            }
            let mut pos = 0;
            while pos < self.fs.block_size {
                let mut header = Ext2DirEntry2Header::decode(&buf, pos)
                    .ok_or(FileSystemError::InvalidFileSystem)?;
                let record = header.rec_len as usize;
                if record < 8 || pos + record > self.fs.block_size {
                    return Err(FileSystemError::InvalidFileSystem);
                }
                let ideal = align_up(Ext2DirEntry2Header::SIZE + header.name_len as usize, 4);
                if header.inode == 0 && record >= needed {
                    header.inode = child;
                    header.name_len = name.len() as u8;
                    header.file_type = inode_kind::file_type(kind);
                    if !header.encode(&mut buf, pos) {
                        return Err(FileSystemError::InvalidFileSystem);
                    }
                    let start = pos + Ext2DirEntry2Header::SIZE;
                    buf[start..start + name.len()].copy_from_slice(name);
                    self.fs.write_fs_block(block, &buf)?;
                    return Ok(());
                }
                if header.inode != 0 && record >= ideal + needed {
                    header.rec_len = ideal as u16;
                    if !header.encode(&mut buf, pos) {
                        return Err(FileSystemError::InvalidFileSystem);
                    }
                    let new_pos = pos + ideal;
                    let new_header = Ext2DirEntry2Header {
                        inode: child,
                        rec_len: (record - ideal) as u16,
                        name_len: name.len() as u8,
                        file_type: inode_kind::file_type(kind),
                    };
                    if !new_header.encode(&mut buf, new_pos) {
                        return Err(FileSystemError::InvalidFileSystem);
                    }
                    let start = new_pos + Ext2DirEntry2Header::SIZE;
                    buf[start..start + name.len()].copy_from_slice(name);
                    self.fs.write_fs_block(block, &buf)?;
                    return Ok(());
                }
                pos += record;
            }
        }
        Err(FileSystemError::NoSpace)
    }

    /// @description 在 active mutation 中删除名称精确匹配的 ext2 dir entry。
    /// @param mutation 证明 caller 持有唯一 active journal transaction；目录不收缩，无 live inode preimage。
    /// @param name 已经 namespace seam 校验的 raw component。
    /// @return 被删除 entry 的 inode number，directory size 不收缩。
    /// @errors entry 不存在、record layout、block mapping、journal 或 I/O 错误。
    pub(super) fn remove_dir_entry_locked(
        &self,
        _mutation: &mut MutationGuard<'_>,
        name: &[u8],
    ) -> Result<u32, FileSystemError> {
        let blocks = ceil_div(self.size() as usize, self.fs.block_size);
        for index in 0..blocks {
            let block = self.map_block(index as u32)?;
            let mut buf = try_zeroed(self.fs.block_size)?;
            let cached = self.fs.read_metadata_block(block)?;
            buf.copy_from_slice(&cached);
            let mut pos = 0;
            let mut previous = None;
            while pos < self.fs.block_size {
                let header = Ext2DirEntry2Header::decode(&buf, pos)
                    .ok_or(FileSystemError::InvalidFileSystem)?;
                let record = header.rec_len as usize;
                if record < 8 || pos + record > self.fs.block_size {
                    return Err(FileSystemError::InvalidFileSystem);
                }
                let start = pos + Ext2DirEntry2Header::SIZE;
                if header.inode != 0
                    && header.name_len as usize <= record - 8
                    && &buf[start..start + header.name_len as usize] == name
                {
                    if let Some(previous_pos) = previous {
                        let mut previous_header = Ext2DirEntry2Header::decode(&buf, previous_pos)
                            .ok_or(FileSystemError::InvalidFileSystem)?;
                        previous_header.rec_len += header.rec_len;
                        if !previous_header.encode(&mut buf, previous_pos) {
                            return Err(FileSystemError::InvalidFileSystem);
                        }
                    } else {
                        let mut empty = header;
                        empty.inode = 0;
                        if !empty.encode(&mut buf, pos) {
                            return Err(FileSystemError::InvalidFileSystem);
                        }
                    }
                    self.fs.write_fs_block(block, &buf)?;
                    return Ok(header.inode);
                }
                previous = Some(pos);
                pos += record;
            }
        }
        Err(FileSystemError::NotFound)
    }

    /// @description 从 opaque byte cursor 所在块开始校验并遍历 ext directory entry。
    /// @param cursor 上次消费 entry 的 next byte offset；stale/misaligned cursor 向后对齐到记录边界。
    /// @param visit 收到 next byte cursor、按值 header 与本次调用内有效的 raw name。
    /// @return 当前已消费 cursor 与 EOF；Stop 不消费当前 entry。
    /// @errors inode size、record layout、block mapping 或 I/O 无效时返回明确错误。
    pub(super) fn dir_iterate_from<F>(
        &self,
        cursor: u64,
        mut visit: F,
    ) -> Result<DirectoryRead, FileSystemError>
    where
        F: FnMut(u64, Ext2DirEntry2Header, &[u8]) -> Result<DirectoryVisit, FileSystemError>,
    {
        let size = self.disk.lock().i_size_lo as usize;
        if !size.is_multiple_of(self.fs.block_size) {
            return Err(FileSystemError::InvalidFileSystem);
        }
        let Ok(start) = usize::try_from(cursor) else {
            return Ok(DirectoryRead { cursor, eof: true });
        };
        if start >= size {
            return Ok(DirectoryRead { cursor, eof: true });
        }
        let mut directory_cursor = DirectoryCursor::new(start, cursor);
        let first_block = directory_cursor.first_block(self.fs.block_size);
        for block_index in first_block..size / self.fs.block_size {
            let block = self
                .map_block(block_index as u32)
                .map_err(|_| FileSystemError::InvalidFileSystem)?;
            let bytes = self.fs.read_metadata_block(block)?;
            let mut offset = 0;
            while offset < self.fs.block_size {
                let header = Ext2DirEntry2Header::decode(&bytes, offset)
                    .ok_or(FileSystemError::InvalidFileSystem)?;
                let record_length = header.rec_len as usize;
                let name_length = header.name_len as usize;
                let minimum = align_up(Ext2DirEntry2Header::SIZE + name_length, 4);
                let end = offset
                    .checked_add(record_length)
                    .ok_or(FileSystemError::InvalidFileSystem)?;
                if record_length < minimum
                    || !record_length.is_multiple_of(4)
                    || end > self.fs.block_size
                {
                    return Err(FileSystemError::InvalidFileSystem);
                }
                let name_start = offset + Ext2DirEntry2Header::SIZE;
                if name_length > 255 || name_start + name_length > end {
                    return Err(FileSystemError::InvalidFileSystem);
                }
                let absolute = block_index * self.fs.block_size + offset;
                let next = block_index * self.fs.block_size + end;
                if directory_cursor.locate(absolute, next) == RecordPosition::Skip {
                    offset = end;
                    continue;
                }
                let next = next as u64;
                match visit(next, header, &bytes[name_start..name_start + name_length])? {
                    DirectoryVisit::Continue => directory_cursor.consume(next),
                    DirectoryVisit::Stop => {
                        return Ok(DirectoryRead {
                            cursor: directory_cursor.published(),
                            eof: false,
                        });
                    }
                }
                offset = end;
            }
        }
        Ok(DirectoryRead {
            cursor: size as u64,
            eof: true,
        })
    }

    /// @description 在同一 mutation transaction 中分配 inode、保存 target 并发布 symlink entry。
    /// @param name 当前目录内的新 entry 名称。
    /// @param target 不含 NUL 的 raw target bytes；不在此解析。
    /// @return 新 Ext2Inode owner。
    /// @errors 类型、名称、重复、空间、内存或 I/O 错误。
    pub(super) fn create_symlink(
        &self,
        name: &[u8],
        target: &[u8],
        metadata: super::super::CreateMetadata,
    ) -> Result<Arc<Self>, FileSystemError> {
        if self.inode_type() != InodeType::Directory {
            return Err(FileSystemError::NotDirectory);
        }
        Self::validate_name(name)?;
        if target.is_empty() {
            return Err(FileSystemError::InvalidPath);
        }
        let mut mutation = self.fs.begin_mutation()?;
        match self.find_child(name) {
            Ok(_) => return Err(FileSystemError::AlreadyExists),
            Err(FileSystemError::NotFound) => {}
            Err(error) => return Err(error),
        }
        let group = self.fs.group_index_and_local_inode(self.inode_num).0;
        let number = self.fs.allocate_inode(group, false)?;
        mutation.discard_inode_on_abort(number)?;
        let now = Self::now();
        let mut disk = Ext2InodeDisk {
            i_mode: 0xA000 | metadata.mode as u16 & 0o7777,
            i_atime: now,
            i_ctime: now,
            i_mtime: now,
            i_links_count: 1,
            ..Default::default()
        };
        disk.set_uid(metadata.uid);
        disk.set_gid(metadata.gid);
        if target.len() <= mem::size_of::<[u32; 15]>() {
            disk.i_size_lo = target.len() as u32;
            if !disk.set_inline_symlink(target) {
                return Err(FileSystemError::InvalidFileSystem);
            }
            self.fs.write_inode_disk(number, &disk)?;
        } else {
            self.fs.write_inode_disk(number, &disk)?;
            Ext2Inode::load(self.fs.clone(), number)?.write_at_locked(&mut mutation, 0, target)?;
        }
        let child = Ext2Inode::load(self.fs.clone(), number)?;
        self.add_dir_entry_locked(&mut mutation, number, name, InodeType::SymLink)?;
        let mut parent = mutation.inode(self)?;
        parent.i_mtime = now;
        parent.i_ctime = now;
        self.fs.write_inode_disk(self.inode_num, &parent)?;
        drop(parent);
        mutation.commit()?;
        Ok(child)
    }

    /// @description 在同一 mutation transaction 中增加 target link count 并发布目录项。
    /// @param name 当前目录内的新 entry 名称。
    /// @param target VFS 保活且已通过 mount identity 检查的目标。
    /// @return mutation journal checkpoint 完成时成功。
    /// @errors 目录目标、跨 filesystem、重复、link limit、空间或 I/O 错误。
    pub(super) fn create_hard_link(
        &self,
        name: &[u8],
        target: Arc<dyn Inode>,
    ) -> Result<(), FileSystemError> {
        if self.inode_type() != InodeType::Directory {
            return Err(FileSystemError::NotDirectory);
        }
        Self::validate_name(name)?;
        if self.filesystem_id() != target.filesystem_id() {
            return Err(FileSystemError::CrossDevice);
        }
        let metadata = target.metadata()?;
        if metadata.kind == InodeType::Directory {
            return Err(FileSystemError::PermissionDenied);
        }
        let mut mutation = self.fs.begin_mutation()?;
        match self.find_child(name) {
            Ok(_) => return Err(FileSystemError::AlreadyExists),
            Err(FileSystemError::NotFound) => {}
            Err(error) => return Err(error),
        }
        let target = Ext2Inode::load(self.fs.clone(), metadata.inode as u32)?;
        let mut target_disk = mutation.inode(&target)?;
        if target_disk.i_links_count == 0 {
            return Err(FileSystemError::NotFound);
        }
        let target_links =
            link_count::increment(target_disk.i_links_count).map_err(link_count_error)?;
        let now = Self::now();
        target_disk.i_links_count = target_links;
        target_disk.i_ctime = now;
        self.fs.write_inode_disk(target.inode_num, &target_disk)?;
        drop(target_disk);
        self.add_dir_entry_locked(&mut mutation, target.inode_num, name, metadata.kind)?;
        let mut parent = mutation.inode(self)?;
        parent.i_mtime = now;
        parent.i_ctime = now;
        self.fs.write_inode_disk(self.inode_num, &parent)?;
        drop(parent);
        mutation.commit()
    }

    /// @description 在唯一 ext2 mutation domain 内完成 rename 与 parent-link net plan。
    pub(super) fn rename_entry(
        &self,
        old_name: &[u8],
        new_parent_inode: u64,
        new_name: &[u8],
        no_replace: bool,
    ) -> Result<(), FileSystemError> {
        if self.inode_type() != InodeType::Directory {
            return Err(FileSystemError::NotDirectory);
        }
        Self::validate_name(old_name)?;
        Self::validate_name(new_name)?;
        let mut mutation = self.fs.begin_mutation()?;
        let new_parent = Ext2Inode::load(self.fs.clone(), new_parent_inode as u32)?;
        if new_parent.inode_type() != InodeType::Directory {
            return Err(FileSystemError::NotDirectory);
        }
        let child = self.find_child(old_name)?;
        if self.inode_num == new_parent.inode_num && old_name == new_name {
            return Ok(());
        }
        let metadata = child.metadata()?;
        if metadata.kind == InodeType::Directory {
            let child_number = metadata.inode as u32;
            let mut ancestor = new_parent.clone();
            let mut reached_root = false;
            for _ in 0..self.fs.superblock.lock().s_inodes_count {
                if ancestor.inode_num == child_number {
                    return Err(FileSystemError::InvalidOperation);
                }
                if ancestor.inode_num == 2 {
                    reached_root = true;
                    break;
                }
                let parent = ancestor.find_child(b"..")?;
                ancestor = Ext2Inode::load(self.fs.clone(), parent.metadata()?.inode as u32)?;
            }
            if !reached_root {
                return Err(FileSystemError::InvalidFileSystem);
            }
        }
        let existing = match new_parent.find_child(new_name) {
            Ok(existing) => Some(existing),
            Err(FileSystemError::NotFound) => None,
            Err(error) => return Err(error),
        };
        let existing_metadata = if let Some(existing) = existing.as_ref() {
            if no_replace {
                return Err(FileSystemError::AlreadyExists);
            }
            let existing_meta = existing.metadata()?;
            if existing_meta.inode == metadata.inode {
                return Ok(());
            }
            if existing_meta.kind == InodeType::Directory && metadata.kind != InodeType::Directory {
                return Err(FileSystemError::IsDirectory);
            }
            if existing_meta.kind != InodeType::Directory && metadata.kind == InodeType::Directory {
                return Err(FileSystemError::NotDirectory);
            }
            if existing_meta.kind == InodeType::Directory && directory_not_empty(existing.as_ref())?
            {
                return Err(FileSystemError::DirectoryNotEmpty);
            }
            Some(existing_meta)
        } else {
            None
        };
        let crosses_parent = self.inode_num != new_parent.inode_num;
        let parent_link_plan = if metadata.kind == InodeType::Directory {
            let old_parent_links = self.disk.lock().i_links_count;
            let new_parent_links = if crosses_parent {
                new_parent.disk.lock().i_links_count
            } else {
                old_parent_links
            };
            link_count::plan_rename_parent_links(
                old_parent_links,
                new_parent_links,
                true,
                crosses_parent,
                existing_metadata.is_some_and(|existing| existing.kind == InodeType::Directory),
            )
            .map_err(link_count_error)?
        } else {
            None
        };
        if let (Some(existing), Some(existing_meta)) = (existing, existing_metadata) {
            new_parent.remove_dir_entry_locked(&mut mutation, new_name)?;
            let (existing, externally_held) =
                self.reload_after_lookup(existing, existing_meta.inode as u32)?;
            let mut disk = mutation.inode(&existing)?;
            if existing_meta.kind != InodeType::Directory && disk.i_links_count > 1 {
                disk.i_links_count =
                    link_count::decrement(disk.i_links_count).map_err(link_count_error)?;
                disk.i_ctime = Self::now();
                self.fs.write_inode_disk(existing.inode_num, &disk)?;
            } else if existing_meta.kind != InodeType::Directory && externally_held {
                drop(disk);
                self.fs.defer_reclaim_locked(&mut mutation, &existing)?;
            } else {
                drop(disk);
                existing
                    .reclaim_locked(&mut mutation, existing_meta.kind == InodeType::Directory)?;
            }
        }
        new_parent.add_dir_entry_locked(
            &mut mutation,
            metadata.inode as u32,
            new_name,
            metadata.kind,
        )?;
        self.remove_dir_entry_locked(&mut mutation, old_name)?;
        {
            let child = Ext2Inode::load(self.fs.clone(), metadata.inode as u32)?;
            let mut disk = mutation.inode(&child)?;
            disk.i_ctime = Self::now();
            self.fs.write_inode_disk(child.inode_num, &disk)?;
        }
        if metadata.kind == InodeType::Directory && crosses_parent {
            let child = Ext2Inode::load(self.fs.clone(), metadata.inode as u32)?;
            child.remove_dir_entry_locked(&mut mutation, b"..")?;
            child.add_dir_entry_locked(
                &mut mutation,
                new_parent.inode_num,
                b"..",
                InodeType::Directory,
            )?;
        }
        let now = Self::now();
        if !crosses_parent {
            let mut disk = mutation.inode(self)?;
            if let Some(link_count::ParentLinkPlan::SameParent { parent }) = parent_link_plan {
                disk.i_links_count = parent;
            }
            disk.i_mtime = now;
            disk.i_ctime = now;
            self.fs.write_inode_disk(self.inode_num, &disk)?;
            drop(disk);
        } else {
            let mut old_disk = mutation.inode(self)?;
            if let Some(link_count::ParentLinkPlan::CrossParent { old_parent, .. }) =
                parent_link_plan
            {
                old_disk.i_links_count = old_parent;
            }
            old_disk.i_mtime = now;
            old_disk.i_ctime = now;
            self.fs.write_inode_disk(self.inode_num, &old_disk)?;
            drop(old_disk);
            let mut new_disk = mutation.inode(&new_parent)?;
            if let Some(link_count::ParentLinkPlan::CrossParent { new_parent, .. }) =
                parent_link_plan
            {
                new_disk.i_links_count = new_parent;
            }
            new_disk.i_mtime = now;
            new_disk.i_ctime = now;
            self.fs.write_inode_disk(new_parent.inode_num, &new_disk)?;
            drop(new_disk);
        }
        mutation.commit()
    }
}
