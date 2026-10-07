use super::directory_block::{insert_record, records, remove_record};
use super::inode::Timestamp;
use super::*;

impl Ext4Inode {
    /// 释放 namespace lookup Arc，重新取得 concrete inode 并冻结真实 external ownership。
    ///
    /// # Returns
    ///
    /// concrete inode 与是否存在除本地 owner 之外的 live Arc。
    ///
    /// # Errors
    ///
    /// inode reload 的 filesystem、I/O 或 allocation 错误。
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

    fn directory_blocks(&self) -> Result<u32, FileSystemError> {
        let size = self.disk.lock().size();
        if !size.is_multiple_of(self.fs.block_size as u64) {
            return Err(FileSystemError::InvalidFileSystem);
        }
        u32::try_from(size / self.fs.block_size as u64)
            .map_err(|_| FileSystemError::InvalidFileSystem)
    }

    /// 按名称查找 entry，返回 inode number 与 dirent file type。
    pub(super) fn lookup_entry(&self, name: &[u8]) -> Result<Option<(u32, u8)>, FileSystemError> {
        if self.is_indexed() {
            return self.dx_lookup(name);
        }
        for logical in 0..self.directory_blocks()? {
            let block = self.read_leaf(self.map_block(logical)?)?;
            for record in records(&block, self.leaf_usable()) {
                let record = record?;
                if record.header.inode != 0 && record.name(&block) == name {
                    return Ok(Some((record.header.inode, record.header.file_type)));
                }
            }
        }
        Ok(None)
    }

    /// 在 active mutation 中插入唯一 entry；单 block 线性目录满时转为 htree。
    pub(super) fn add_dir_entry_locked(
        &self,
        mutation: &mut MutationGuard<'_>,
        child: u32,
        name: &[u8],
        kind: InodeType,
    ) -> Result<(), FileSystemError> {
        let file_type = inode_kind::file_type(kind);
        if self.is_indexed() {
            return self.dx_add(mutation, child, name, file_type);
        }
        let blocks = self.directory_blocks()?;
        let usable = self.leaf_usable();
        let mut first_block = None;
        for logical in 0..blocks {
            let physical = self.map_block(logical)?;
            let cached = self.read_leaf(physical)?;
            let mut block = try_zeroed(self.fs.block_size)?;
            block.copy_from_slice(&cached);
            if insert_record(&mut block, usable, child, name, file_type)? {
                return self.write_leaf(physical, &mut block);
            }
            if logical == 0 {
                first_block = Some((physical, block));
            }
        }
        if blocks == 1 {
            let (physical, block) = first_block.expect("single-block directory was scanned");
            self.make_indexed(mutation, physical, &block)?;
            return self.dx_add(mutation, child, name, file_type);
        }
        let (_, physical) = self.append_directory_block(mutation)?;
        let mut block = self.empty_leaf()?;
        if !insert_record(&mut block, usable, child, name, file_type)? {
            return Err(FileSystemError::InvalidFileSystem);
        }
        self.write_leaf(physical, &mut block)
    }

    /// 为新 directory 写入首个包含 `.` 与 `..` 的 leaf。
    pub(super) fn initialize_directory_locked(
        &self,
        mutation: &mut MutationGuard<'_>,
        parent: u32,
    ) -> Result<(), FileSystemError> {
        let (_, physical) = self.append_directory_block(mutation)?;
        let directory = inode_kind::file_type(InodeType::Directory);
        let mut block = try_zeroed(self.fs.block_size)?;
        directory_block::compact_records(
            &mut block,
            self.leaf_usable(),
            &[
                (self.inode_num, directory, b"."),
                (parent, directory, b".."),
            ],
        )?;
        self.write_leaf(physical, &mut block)
    }

    /// 在 active mutation 中删除名称精确匹配的 entry；directory 不收缩。
    ///
    /// # Errors
    ///
    /// entry 不存在、record layout、block mapping、journal 或 I/O 错误。
    pub(super) fn remove_dir_entry_locked(
        &self,
        _mutation: &mut MutationGuard<'_>,
        name: &[u8],
    ) -> Result<u32, FileSystemError> {
        if self.is_indexed() {
            return self.dx_remove(name)?.ok_or(FileSystemError::NotFound);
        }
        for logical in 0..self.directory_blocks()? {
            let physical = self.map_block(logical)?;
            let cached = self.read_leaf(physical)?;
            let mut block = try_zeroed(self.fs.block_size)?;
            block.copy_from_slice(&cached);
            if let Some(inode) = remove_record(&mut block, self.leaf_usable(), name)? {
                self.write_leaf(physical, &mut block)?;
                return Ok(inode);
            }
        }
        Err(FileSystemError::NotFound)
    }

    /// 从 opaque byte cursor 所在块开始遍历线性 directory。
    ///
    /// # Parameters
    ///
    /// - `cursor`: 上次消费 entry 的 next byte offset；stale/misaligned cursor 向后对齐到记录边界。
    /// - `visit`: 收到 next byte cursor、inode、file type 与本次调用内有效的 raw name。
    ///
    /// # Returns
    ///
    /// 当前已消费 cursor 与 EOF；Stop 不消费当前 entry。
    pub(super) fn dir_iterate_from(
        &self,
        cursor: u64,
        visit: &mut EntryVisitor<'_>,
    ) -> Result<DirectoryRead, FileSystemError> {
        let size = usize::try_from(self.directory_blocks()?)
            .map_err(|_| FileSystemError::InvalidFileSystem)?
            * self.fs.block_size;
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
            let bytes = self.read_leaf(block)?;
            for record in records(&bytes, self.leaf_usable()) {
                let record = record?;
                let absolute = block_index * self.fs.block_size + record.offset;
                let next = block_index * self.fs.block_size + record.end();
                if directory_cursor.locate(absolute, next) == RecordPosition::Skip
                    || record.header.inode == 0
                {
                    continue;
                }
                let next = next as u64;
                match visit(
                    next,
                    record.header.inode,
                    record.header.file_type,
                    record.name(&bytes),
                )? {
                    DirectoryVisit::Continue => directory_cursor.consume(next),
                    DirectoryVisit::Stop => {
                        return Ok(DirectoryRead {
                            cursor: directory_cursor.published(),
                            eof: false,
                        });
                    }
                }
            }
        }
        Ok(DirectoryRead {
            cursor: size as u64,
            eof: true,
        })
    }

    /// 在同一 mutation transaction 中分配 inode、保存 target 并发布 symlink entry。
    ///
    /// # Errors
    ///
    /// 类型、名称、重复、空间、内存或 I/O 错误。
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
        if self.lookup_entry(name)?.is_some() {
            return Err(FileSystemError::AlreadyExists);
        }
        let group = self.fs.group_index_and_local_inode(self.inode_num)?.0;
        let number = self.fs.allocate_inode(group, false)?;
        mutation.discard_inode_on_abort(number)?;
        // Linux ext4：短于 60 byte 的 target 是 fast symlink，存于 i_block 且不带 EXTENTS_FL。
        let fast = target.len() < layout::INODE_BLOCK_BYTES;
        let mut disk = Self::new_disk(
            &self.fs,
            0xA000 | metadata.mode as u16 & 0o7777,
            metadata.uid,
            metadata.gid,
            1,
            !fast,
        );
        if fast {
            disk.set_size(target.len() as u64);
            if !disk.set_inline_symlink(target) {
                return Err(FileSystemError::InvalidFileSystem);
            }
            self.fs.write_inode_disk(number, &disk)?;
        } else {
            self.fs.write_inode_disk(number, &disk)?;
            Ext4Inode::load(self.fs.clone(), number)?.write_at_locked(&mut mutation, 0, target)?;
        }
        let child = Ext4Inode::load(self.fs.clone(), number)?;
        self.add_dir_entry_locked(&mut mutation, number, name, InodeType::SymLink)?;
        self.touch_parent(&mut mutation, None)?;
        mutation.commit()?;
        Ok(child)
    }

    /// 更新 parent 的 mtime/ctime 与可选 link count。
    pub(super) fn touch_parent(
        &self,
        mutation: &mut MutationGuard<'_>,
        links: Option<u16>,
    ) -> Result<(), FileSystemError> {
        let now = Timestamp::now();
        let mut parent = mutation.inode(self)?;
        if let Some(links) = links {
            parent.i_links_count = links;
        }
        parent.set_mtime(now);
        parent.set_ctime(now);
        self.fs.write_inode_disk(self.inode_num, &parent)
    }

    /// 在同一 mutation transaction 中增加 target link count 并发布目录项。
    ///
    /// # Errors
    ///
    /// 目录目标、跨 filesystem、重复、link limit、空间或 I/O 错误。
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
        if self.lookup_entry(name)?.is_some() {
            return Err(FileSystemError::AlreadyExists);
        }
        let target = Ext4Inode::load(self.fs.clone(), metadata.inode as u32)?;
        let mut target_disk = mutation.inode(&target)?;
        if target_disk.i_links_count == 0 {
            return Err(FileSystemError::NotFound);
        }
        target_disk.i_links_count =
            link_count::increment_file(target_disk.i_links_count).map_err(link_count_error)?;
        target_disk.set_ctime(Timestamp::now());
        self.fs.write_inode_disk(target.inode_num, &target_disk)?;
        drop(target_disk);
        self.add_dir_entry_locked(&mut mutation, target.inode_num, name, metadata.kind)?;
        self.touch_parent(&mut mutation, None)?;
        mutation.commit()
    }

    /// 在唯一 ext4 mutation domain 内完成 rename 与 parent-link net plan。
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
        let new_parent = Ext4Inode::load(self.fs.clone(), new_parent_inode as u32)?;
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
                if ancestor.inode_num == EXT4_ROOT_INO {
                    reached_root = true;
                    break;
                }
                let (parent, _) = ancestor
                    .lookup_entry(b"..")?
                    .ok_or(FileSystemError::InvalidFileSystem)?;
                ancestor = Ext4Inode::load(self.fs.clone(), parent)?;
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
                    link_count::decrement_file(disk.i_links_count).map_err(link_count_error)?;
                disk.set_ctime(Timestamp::now());
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
        let child = Ext4Inode::load(self.fs.clone(), metadata.inode as u32)?;
        {
            let mut disk = mutation.inode(&child)?;
            disk.set_ctime(Timestamp::now());
            self.fs.write_inode_disk(child.inode_num, &disk)?;
        }
        if metadata.kind == InodeType::Directory && crosses_parent {
            child.set_parent_entry(new_parent.inode_num)?;
        }
        match parent_link_plan {
            Some(link_count::ParentLinkPlan::SameParent { parent }) => {
                self.touch_parent(&mut mutation, Some(parent))?;
            }
            Some(link_count::ParentLinkPlan::CrossParent {
                old_parent,
                new_parent: new_links,
            }) => {
                self.touch_parent(&mut mutation, Some(old_parent))?;
                new_parent.touch_parent(&mut mutation, Some(new_links))?;
            }
            None => {
                self.touch_parent(&mut mutation, None)?;
                if crosses_parent {
                    new_parent.touch_parent(&mut mutation, None)?;
                }
            }
        }
        mutation.commit()
    }
}
