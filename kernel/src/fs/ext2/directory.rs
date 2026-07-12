use super::*;

const EXT2_LINK_MAX: u16 = 32_000;

impl Ext2Inode {
    /// @description 逐块校验并遍历当前目录的全部 ext directory entry。
    /// @param visit 收到按值 header 与本次调用内有效的 raw name；返回 false 提前结束。
    /// @return 遍历完成或 callback 主动停止时成功。
    /// @errors inode size、record layout、block mapping 或 I/O 无效时返回明确错误。
    pub(super) fn dir_iterate_blocks<F: FnMut(Ext2DirEntry2Header, &[u8]) -> bool>(
        &self,
        mut visit: F,
    ) -> Result<(), FileSystemError> {
        let size = self.disk.lock().i_size_lo as usize;
        if size % self.fs.block_size != 0 {
            return Err(FileSystemError::InvalidFileSystem);
        }
        for block_index in 0..size / self.fs.block_size {
            let block = self
                .map_block(block_index as u32)
                .map_err(|_| FileSystemError::InvalidFileSystem)?;
            let mut bytes = vec![0u8; self.fs.block_size];
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
                if record_length < minimum || record_length % 4 != 0 || end > self.fs.block_size {
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
        let mutation = self.fs.begin_mutation()?;
        match self.find_child(name) {
            Ok(_) => return Err(FileSystemError::AlreadyExists),
            Err(FileSystemError::NotFound) => {}
            Err(error) => return Err(error),
        }
        let group = self.fs.group_index_and_local_inode(self.inode_num).0;
        let number = self.fs.allocate_inode(group, false)?;
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
            Ext2Inode::load(self.fs.clone(), number)?.write_at_locked(0, target)?;
        }
        let child = Ext2Inode::load(self.fs.clone(), number)?;
        self.add_dir_entry_locked(number, name, InodeType::SymLink)?;
        let mut parent = self.disk.lock();
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
        let mutation = self.fs.begin_mutation()?;
        match self.find_child(name) {
            Ok(_) => return Err(FileSystemError::AlreadyExists),
            Err(FileSystemError::NotFound) => {}
            Err(error) => return Err(error),
        }
        let target = Ext2Inode::load(self.fs.clone(), metadata.inode as u32)?;
        let mut target_disk = target.disk.lock();
        if target_disk.i_links_count == 0 {
            return Err(FileSystemError::NotFound);
        }
        if target_disk.i_links_count >= EXT2_LINK_MAX {
            return Err(FileSystemError::TooManyLinks);
        }
        self.add_dir_entry_locked(target.inode_num, name, metadata.kind)?;
        let now = Self::now();
        target_disk.i_links_count += 1;
        target_disk.i_ctime = now;
        self.fs.write_inode_disk(target.inode_num, &target_disk)?;
        let mut parent = self.disk.lock();
        parent.i_mtime = now;
        parent.i_ctime = now;
        self.fs.write_inode_disk(self.inode_num, &parent)?;
        drop(parent);
        drop(target_disk);
        mutation.commit()
    }
}
