use super::*;

const EXT2_LINK_MAX: u16 = 32_000;

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
        let needed = align_up(mem::size_of::<Ext2DirEntry2Header>() + name.len(), 4);
        let blocks = ceil_div(self.size() as usize, self.fs.block_size);
        for index in 0..=blocks {
            let block = if index == blocks {
                self.ensure_block_mapped(mutation, index as u32)?
            } else {
                self.map_block(index as u32)?
            };
            let mut buf = try_zeroed(self.fs.block_size)?;
            if index < blocks {
                self.fs.read_fs_block(block, &mut buf)?;
            }
            if index == blocks {
                let header = Ext2DirEntry2Header {
                    inode: child,
                    rec_len: self.fs.block_size as u16,
                    name_len: name.len() as u8,
                    file_type: Self::file_type(kind),
                };
                // SAFETY: a fresh complete block has room for the header and validated name.
                unsafe {
                    ptr::write_unaligned(buf.as_mut_ptr() as *mut Ext2DirEntry2Header, header)
                };
                buf[mem::size_of::<Ext2DirEntry2Header>()
                    ..mem::size_of::<Ext2DirEntry2Header>() + name.len()]
                    .copy_from_slice(name);
                self.fs.write_fs_block(block, &buf)?;
                let mut inode = mutation.inode(self)?;
                Self::set_disk_size(&mut inode, ((index + 1) * self.fs.block_size) as u64);
                self.fs.write_inode_disk(self.inode_num, &inode)?;
                return Ok(());
            }
            let mut pos = 0;
            while pos < self.fs.block_size {
                // SAFETY: directory validation guarantees a complete header at pos.
                let mut header = unsafe {
                    ptr::read_unaligned(buf.as_ptr().add(pos) as *const Ext2DirEntry2Header)
                };
                let record = header.rec_len as usize;
                if record < 8 || pos + record > self.fs.block_size {
                    return Err(FileSystemError::InvalidFileSystem);
                }
                let ideal = align_up(
                    mem::size_of::<Ext2DirEntry2Header>() + header.name_len as usize,
                    4,
                );
                if header.inode == 0 && record >= needed {
                    header.inode = child;
                    header.name_len = name.len() as u8;
                    header.file_type = Self::file_type(kind);
                    // SAFETY: directory validation proved `record` covers a complete header at
                    // `pos`; write_unaligned updates that on-disk header without forming a reference.
                    unsafe {
                        ptr::write_unaligned(
                            buf.as_mut_ptr().add(pos) as *mut Ext2DirEntry2Header,
                            header,
                        )
                    };
                    let start = pos + mem::size_of::<Ext2DirEntry2Header>();
                    buf[start..start + name.len()].copy_from_slice(name);
                    self.fs.write_fs_block(block, &buf)?;
                    return Ok(());
                }
                if header.inode != 0 && record >= ideal + needed {
                    header.rec_len = ideal as u16;
                    // SAFETY: `pos` names the validated current record and its complete header
                    // lies inside the full block buffer.
                    unsafe {
                        ptr::write_unaligned(
                            buf.as_mut_ptr().add(pos) as *mut Ext2DirEntry2Header,
                            header,
                        )
                    };
                    let new_pos = pos + ideal;
                    let new_header = Ext2DirEntry2Header {
                        inode: child,
                        rec_len: (record - ideal) as u16,
                        name_len: name.len() as u8,
                        file_type: Self::file_type(kind),
                    };
                    // SAFETY: split condition proves `new_pos + header_size <= pos + record`, so
                    // the new unaligned header lies wholly inside the current block buffer.
                    unsafe {
                        ptr::write_unaligned(
                            buf.as_mut_ptr().add(new_pos) as *mut Ext2DirEntry2Header,
                            new_header,
                        )
                    };
                    let start = new_pos + mem::size_of::<Ext2DirEntry2Header>();
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
            self.fs.read_fs_block(block, &mut buf)?;
            let mut pos = 0;
            let mut previous = None;
            while pos < self.fs.block_size {
                // SAFETY: prior record validation advances `pos` by a nonzero aligned rec_len;
                // the loop bound and filesystem validation guarantee a complete header remains.
                let header = unsafe {
                    ptr::read_unaligned(buf.as_ptr().add(pos) as *const Ext2DirEntry2Header)
                };
                let record = header.rec_len as usize;
                if record < 8 || pos + record > self.fs.block_size {
                    return Err(FileSystemError::InvalidFileSystem);
                }
                let start = pos + mem::size_of::<Ext2DirEntry2Header>();
                if header.inode != 0
                    && header.name_len as usize <= record - 8
                    && &buf[start..start + header.name_len as usize] == name
                {
                    if let Some(previous_pos) = previous {
                        // SAFETY: `previous_pos` was recorded only after validating a complete
                        // preceding directory record in this same live block buffer.
                        let mut previous_header = unsafe {
                            ptr::read_unaligned(
                                buf.as_ptr().add(previous_pos) as *const Ext2DirEntry2Header
                            )
                        };
                        previous_header.rec_len += header.rec_len;
                        // SAFETY: previous header remains inside the block; merging adjacent
                        // validated lengths cannot extend beyond their original combined span.
                        unsafe {
                            ptr::write_unaligned(
                                buf.as_mut_ptr().add(previous_pos) as *mut Ext2DirEntry2Header,
                                previous_header,
                            )
                        };
                    } else {
                        let mut empty = header;
                        empty.inode = 0;
                        // SAFETY: `pos` currently identifies a validated complete header in buf;
                        // write_unaligned changes only its inode field representation.
                        unsafe {
                            ptr::write_unaligned(
                                buf.as_mut_ptr().add(pos) as *mut Ext2DirEntry2Header,
                                empty,
                            )
                        };
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

    /// @description 逐块校验并遍历当前目录的全部 ext directory entry。
    /// @param visit 收到按值 header 与本次调用内有效的 raw name；返回 false 提前结束。
    /// @return 遍历完成或 callback 主动停止时成功。
    /// @errors inode size、record layout、block mapping 或 I/O 无效时返回明确错误。
    pub(super) fn dir_iterate_blocks<F: FnMut(Ext2DirEntry2Header, &[u8]) -> bool>(
        &self,
        mut visit: F,
    ) -> Result<(), FileSystemError> {
        let size = self.disk.lock().i_size_lo as usize;
        if !size.is_multiple_of(self.fs.block_size) {
            return Err(FileSystemError::InvalidFileSystem);
        }
        for block_index in 0..size / self.fs.block_size {
            let block = self
                .map_block(block_index as u32)
                .map_err(|_| FileSystemError::InvalidFileSystem)?;
            let mut bytes = super::try_zeroed(self.fs.block_size)?;
            self.fs.read_fs_block(block, &mut bytes)?;
            let mut offset = 0;
            while offset < self.fs.block_size {
                if offset + mem::size_of::<Ext2DirEntry2Header>() > self.fs.block_size {
                    return Err(FileSystemError::InvalidFileSystem);
                }
                // SAFETY: 剩余 bytes 已覆盖完整 packed header；按值非对齐读取。
                let header = unsafe {
                    ptr::read_unaligned(bytes[offset..].as_ptr() as *const Ext2DirEntry2Header)
                };
                let record_length = header.rec_len as usize;
                let name_length = header.name_len as usize;
                let minimum = align_up(mem::size_of::<Ext2DirEntry2Header>() + name_length, 4);
                let end = offset
                    .checked_add(record_length)
                    .ok_or(FileSystemError::InvalidFileSystem)?;
                if record_length < minimum
                    || !record_length.is_multiple_of(4)
                    || end > self.fs.block_size
                {
                    return Err(FileSystemError::InvalidFileSystem);
                }
                let name_start = offset + mem::size_of::<Ext2DirEntry2Header>();
                if name_length > 255 || name_start + name_length > end {
                    return Err(FileSystemError::InvalidFileSystem);
                }
                if !visit(header, &bytes[name_start..name_start + name_length]) {
                    return Ok(());
                }
                offset = end;
            }
        }
        Ok(())
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
            // SAFETY: target 不超过 i_block 的 60-byte inline storage，且源/目标不重叠。
            unsafe {
                ptr::copy_nonoverlapping(
                    target.as_ptr(),
                    ptr::addr_of_mut!(disk.i_block).cast::<u8>(),
                    target.len(),
                )
            };
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
        if target_disk.i_links_count >= EXT2_LINK_MAX {
            return Err(FileSystemError::TooManyLinks);
        }
        self.add_dir_entry_locked(&mut mutation, target.inode_num, name, metadata.kind)?;
        let now = Self::now();
        target_disk.i_links_count += 1;
        target_disk.i_ctime = now;
        self.fs.write_inode_disk(target.inode_num, &target_disk)?;
        let mut parent = mutation.inode(self)?;
        parent.i_mtime = now;
        parent.i_ctime = now;
        self.fs.write_inode_disk(self.inode_num, &parent)?;
        drop(parent);
        drop(target_disk);
        mutation.commit()
    }
}
