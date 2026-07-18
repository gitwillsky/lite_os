use super::*;

mod vfs;

#[derive(Debug)]
pub(super) struct Ext2Inode {
    pub(super) fs: Arc<Ext2FileSystem>,
    pub(super) inode_num: u32,
    pub(super) disk: Mutex<Ext2InodeDisk>,
}

impl Ext2Inode {
    pub(super) fn load(
        fs: Arc<Ext2FileSystem>,
        inode_num: u32,
    ) -> Result<Arc<Self>, FileSystemError> {
        if let Some(inode) = fs
            .inode_cache
            .lock()
            .get(&inode_num)
            .and_then(Weak::upgrade)
        {
            return Ok(inode);
        }
        let disk = fs.read_inode_disk(inode_num)?;
        let cache_slot = FallibleMap::<u32, Weak<Ext2Inode>>::try_reserve_node()
            .map_err(|_| FileSystemError::OutOfMemory)?;
        let inode = Arc::try_new(Self {
            fs,
            inode_num,
            disk: Mutex::new(disk),
        })
        .map_err(|_| FileSystemError::OutOfMemory)?;
        let mut cache = inode.fs.inode_cache.lock();
        if let Some(existing) = cache.get(&inode_num).and_then(Weak::upgrade) {
            return Ok(existing);
        }
        cache.remove(&inode_num);
        cache.commit_vacant(cache_slot.fill(inode_num, Arc::downgrade(&inode)));
        drop(cache);
        Ok(inode)
    }

    pub(super) fn disk_size(inode: &Ext2InodeDisk) -> u64 {
        let low = inode.i_size_lo as u64;
        if inode_kind::from_mode(inode.i_mode) == InodeType::File {
            low | ((inode.i_dir_acl_or_size_high as u64) << 32)
        } else {
            low
        }
    }

    pub(super) fn set_disk_size(inode: &mut Ext2InodeDisk, size: u64) {
        inode.i_size_lo = size as u32;
        inode.i_dir_acl_or_size_high = if inode_kind::from_mode(inode.i_mode) == InodeType::File {
            (size >> 32) as u32
        } else {
            0
        };
    }

    pub(super) fn now() -> u32 {
        (crate::timer::get_realtime_ns() / 1_000_000_000) as u32
    }

    pub(super) fn validate_name(name: &[u8]) -> Result<(), FileSystemError> {
        if name.is_empty()
            || name.len() > 255
            || name == b"."
            || name == b".."
            || name.contains(&b'/')
            || name.contains(&0)
        {
            return Err(FileSystemError::InvalidPath);
        }
        Ok(())
    }

    pub(super) fn read_pointer_block(&self, block: u32) -> Result<Vec<u32>, FileSystemError> {
        let mut raw = try_zeroed(self.fs.block_size)?;
        self.fs.read_fs_block(block, &mut raw)?;
        let mut pointers = Vec::new();
        pointers
            .try_reserve_exact(self.fs.block_size / 4)
            .map_err(|_| FileSystemError::OutOfMemory)?;
        for chunk in raw.as_chunks::<4>().0 {
            pointers.push(u32::from_le_bytes(*chunk));
        }
        Ok(pointers)
    }

    pub(super) fn write_pointer_block(
        &self,
        block: u32,
        pointers: &[u32],
    ) -> Result<(), FileSystemError> {
        let mut raw = try_zeroed(self.fs.block_size)?;
        for (chunk, pointer) in raw.as_chunks_mut::<4>().0.iter_mut().zip(pointers) {
            chunk.copy_from_slice(&pointer.to_le_bytes());
        }
        self.fs.write_fs_block(block, &raw)
    }

    pub(super) fn pointer_path(
        &self,
        file_block: u32,
    ) -> Result<(usize, Vec<usize>), FileSystemError> {
        let count = self.fs.block_size / 4;
        let mut index = file_block as usize;
        if index < 12 {
            return Ok((index, Vec::new()));
        }
        index -= 12;
        if index < count {
            return Ok((12, try_indices(&[index])?));
        }
        index -= count;
        if index < count * count {
            return Ok((13, try_indices(&[index / count, index % count])?));
        }
        index -= count * count;
        if index < count * count * count {
            return Ok((
                14,
                try_indices(&[
                    index / (count * count),
                    index / count % count,
                    index % count,
                ])?,
            ));
        }
        Err(FileSystemError::NoSpace)
    }

    fn free_tree(&self, block: u32, level: usize) -> Result<u32, FileSystemError> {
        let mut sectors = (self.fs.block_size / 512) as u32;
        if level > 0 {
            for pointer in self.read_pointer_block(block)? {
                if pointer != 0 {
                    sectors += self.free_tree(pointer, level - 1)?;
                }
            }
        }
        self.fs.free_block(block)?;
        Ok(sectors)
    }

    fn trim_tree(
        &self,
        block: u32,
        level: usize,
        logical_base: usize,
        keep_blocks: usize,
    ) -> Result<(bool, u32), FileSystemError> {
        let count = self.fs.block_size / 4;
        let child_span = count.pow((level - 1) as u32);
        let mut pointers = self.read_pointer_block(block)?;
        let mut freed = 0;
        for (index, pointer) in pointers.iter_mut().enumerate() {
            if *pointer == 0 {
                continue;
            }
            let base = logical_base + index * child_span;
            if base >= keep_blocks {
                freed += self.free_tree(*pointer, level - 1)?;
                *pointer = 0;
            } else if level > 1 && base + child_span > keep_blocks {
                let (empty, child_freed) =
                    self.trim_tree(*pointer, level - 1, base, keep_blocks)?;
                freed += child_freed;
                if empty {
                    self.fs.free_block(*pointer)?;
                    freed += (self.fs.block_size / 512) as u32;
                    *pointer = 0;
                }
            }
        }
        let empty = pointers.iter().all(|pointer| *pointer == 0);
        if !empty {
            self.write_pointer_block(block, &pointers)?;
        }
        Ok((empty, freed))
    }

    fn truncate_locked(
        &self,
        mutation: &mut MutationGuard<'_>,
        size: u64,
    ) -> Result<(), FileSystemError> {
        if self.inode_type() == InodeType::Directory {
            return Err(FileSystemError::IsDirectory);
        }
        let old_size = self.size();
        if self.inode_type() == InodeType::SymLink && old_size <= mem::size_of::<[u32; 15]>() as u64
        {
            if size != 0 {
                return Err(FileSystemError::InvalidOperation);
            }
            let mut inode = mutation.inode(self)?;
            inode.i_block = [0; 15];
            Self::set_disk_size(&mut inode, 0);
            inode.i_mtime = Self::now();
            inode.i_ctime = inode.i_mtime;
            return self.fs.write_inode_disk(self.inode_num, &inode);
        }
        if size < old_size {
            let keep = ceil_div(size as usize, self.fs.block_size);
            let mut inode = mutation.inode(self)?;
            let mut freed = 0u32;
            for index in keep..12 {
                if inode.i_block[index] != 0 {
                    freed += self.free_tree(inode.i_block[index], 0)?;
                    inode.i_block[index] = 0;
                }
            }
            let count = self.fs.block_size / 4;
            let roots = [
                (12, 1, 12),
                (13, 2, 12 + count),
                (14, 3, 12 + count + count * count),
            ];
            for (slot, level, base) in roots {
                if inode.i_block[slot] == 0 {
                    continue;
                }
                let (empty, child_freed) =
                    self.trim_tree(inode.i_block[slot], level, base, keep)?;
                freed += child_freed;
                if empty {
                    self.fs.free_block(inode.i_block[slot])?;
                    freed += (self.fs.block_size / 512) as u32;
                    inode.i_block[slot] = 0;
                }
            }
            inode.i_blocks_lo = inode
                .i_blocks_lo
                .checked_sub(freed)
                .ok_or(FileSystemError::InvalidFileSystem)?;
            if !size.is_multiple_of(self.fs.block_size as u64) && keep != 0 {
                drop(inode);
                let block = self.map_block_sparse((keep - 1) as u32)?;
                if block != 0 {
                    let mut data = try_zeroed(self.fs.block_size)?;
                    self.fs.read_fs_block(block, &mut data)?;
                    data[size as usize % self.fs.block_size..].fill(0);
                    self.fs.write_fs_block(block, &data)?;
                }
                inode = mutation.inode(self)?;
            }
            Self::set_disk_size(&mut inode, size);
            inode.i_mtime = Self::now();
            inode.i_ctime = inode.i_mtime;
            self.fs.write_inode_disk(self.inode_num, &inode)?;
        } else if size > old_size {
            let mut inode = mutation.inode(self)?;
            Self::set_disk_size(&mut inode, size);
            inode.i_mtime = Self::now();
            inode.i_ctime = inode.i_mtime;
            self.fs.write_inode_disk(self.inode_num, &inode)?;
        }
        Ok(())
    }

    pub(super) fn reclaim_locked(
        &self,
        mutation: &mut MutationGuard<'_>,
        directory: bool,
    ) -> Result<(), FileSystemError> {
        if directory {
            mutation.inode(self)?.i_mode = 0x8000;
        }
        self.truncate_locked(mutation, 0)?;
        let mut disk = mutation.inode(self)?;
        *disk = Ext2InodeDisk::default();
        self.fs.write_inode_disk(self.inode_num, &disk)?;
        drop(disk);
        self.fs.free_inode(self.inode_num, directory)
    }

    pub(super) fn map_block(&self, file_block_index: u32) -> Result<u32, FileSystemError> {
        let ino = self.disk.lock();
        let ptrs_per_block = (self.fs.block_size / 4) as u32;

        // Direct blocks (0-11)
        if file_block_index < 12 {
            let b = ino.i_block[file_block_index as usize];
            if b == 0 {
                return Err(FileSystemError::NotFound);
            }
            return Ok(b);
        }

        let mut idx = file_block_index - 12;

        // Single indirect blocks (12 - 12 + ptrs_per_block - 1)
        if idx < ptrs_per_block {
            let ind = ino.i_block[12];
            if ind == 0 {
                return Err(FileSystemError::NotFound);
            }
            drop(ino);
            return self.read_indirect_block_pointer(ind, idx);
        }

        idx -= ptrs_per_block;

        // Double indirect blocks (12 + ptrs_per_block to 12 + ptrs_per_block + ptrs_per_block^2 - 1)
        if idx < ptrs_per_block * ptrs_per_block {
            let double_ind = ino.i_block[13];
            if double_ind == 0 {
                return Err(FileSystemError::NotFound);
            }
            drop(ino);

            let first_level_idx = idx / ptrs_per_block;
            let second_level_idx = idx % ptrs_per_block;

            // Read first level indirect block to get second level indirect block
            let single_ind = self.read_indirect_block_pointer(double_ind, first_level_idx)?;

            // Read second level indirect block to get data block
            return self.read_indirect_block_pointer(single_ind, second_level_idx);
        }

        idx -= ptrs_per_block * ptrs_per_block;

        // Triple indirect blocks
        if idx < ptrs_per_block * ptrs_per_block * ptrs_per_block {
            let triple_ind = ino.i_block[14];
            if triple_ind == 0 {
                return Err(FileSystemError::NotFound);
            }
            drop(ino);

            let first_level_idx = idx / (ptrs_per_block * ptrs_per_block);
            let remaining = idx % (ptrs_per_block * ptrs_per_block);
            let second_level_idx = remaining / ptrs_per_block;
            let third_level_idx = remaining % ptrs_per_block;

            // Read first level to get double indirect block
            let double_ind = self.read_indirect_block_pointer(triple_ind, first_level_idx)?;

            // Read second level to get single indirect block
            let single_ind = self.read_indirect_block_pointer(double_ind, second_level_idx)?;

            // Read third level to get data block
            return self.read_indirect_block_pointer(single_ind, third_level_idx);
        }

        Err(FileSystemError::NotFound)
    }

    /// Helper function to read a pointer from an indirect block
    fn read_indirect_block_pointer(
        &self,
        indirect_block: u32,
        index: u32,
    ) -> Result<u32, FileSystemError> {
        let mut buf = try_zeroed(self.fs.block_size)?;
        self.fs.read_fs_block(indirect_block, &mut buf)?;

        let offset = index as usize * 4;
        if offset + 4 > buf.len() {
            return Err(FileSystemError::InvalidFileSystem);
        }

        let block_ptr = u32::from_le_bytes(buf[offset..offset + 4].try_into().unwrap());

        if block_ptr == 0 {
            return Err(FileSystemError::NotFound);
        }

        Ok(block_ptr)
    }

    /// Map block for sparse files - returns Ok(0) for holes instead of error
    pub(super) fn map_block_sparse(&self, file_block_index: u32) -> Result<u32, FileSystemError> {
        let ino = self.disk.lock();
        let ptrs_per_block = (self.fs.block_size / 4) as u32;

        // Direct blocks (0-11)
        if file_block_index < 12 {
            let b = ino.i_block[file_block_index as usize];
            return Ok(b); // Return 0 for holes, actual block number for allocated blocks
        }

        let mut idx = file_block_index - 12;

        // Single indirect blocks
        if idx < ptrs_per_block {
            let ind = ino.i_block[12];
            if ind == 0 {
                return Ok(0); // Hole - indirect block not allocated
            }
            drop(ino);

            match self.read_indirect_block_pointer(ind, idx) {
                Ok(block_ptr) => Ok(block_ptr),
                Err(FileSystemError::NotFound) => Ok(0), // Hole
                Err(e) => Err(e),
            }
        }
        // Double indirect blocks
        else if idx < ptrs_per_block * ptrs_per_block {
            idx -= ptrs_per_block;
            let double_ind = ino.i_block[13];
            if double_ind == 0 {
                return Ok(0); // Hole - double indirect block not allocated
            }
            drop(ino);

            let first_level_idx = idx / ptrs_per_block;
            let second_level_idx = idx % ptrs_per_block;

            // Read first level indirect block
            let single_ind = match self.read_indirect_block_pointer(double_ind, first_level_idx) {
                Ok(ptr) => ptr,
                Err(FileSystemError::NotFound) => return Ok(0), // Hole
                Err(e) => return Err(e),
            };

            if single_ind == 0 {
                return Ok(0); // Hole
            }

            // Read second level indirect block
            match self.read_indirect_block_pointer(single_ind, second_level_idx) {
                Ok(block_ptr) => Ok(block_ptr),
                Err(FileSystemError::NotFound) => Ok(0), // Hole
                Err(e) => Err(e),
            }
        }
        // Triple indirect blocks
        else {
            idx -= ptrs_per_block * ptrs_per_block;
            if idx >= ptrs_per_block * ptrs_per_block * ptrs_per_block {
                return Ok(0); // Beyond maximum file size
            }

            let triple_ind = ino.i_block[14];
            if triple_ind == 0 {
                return Ok(0); // Hole
            }
            drop(ino);

            let first_level_idx = idx / (ptrs_per_block * ptrs_per_block);
            let remaining = idx % (ptrs_per_block * ptrs_per_block);
            let second_level_idx = remaining / ptrs_per_block;
            let third_level_idx = remaining % ptrs_per_block;

            // Read first level
            let double_ind = match self.read_indirect_block_pointer(triple_ind, first_level_idx) {
                Ok(ptr) => ptr,
                Err(FileSystemError::NotFound) => return Ok(0), // Hole
                Err(e) => return Err(e),
            };

            if double_ind == 0 {
                return Ok(0); // Hole
            }

            // Read second level
            let single_ind = match self.read_indirect_block_pointer(double_ind, second_level_idx) {
                Ok(ptr) => ptr,
                Err(FileSystemError::NotFound) => return Ok(0), // Hole
                Err(e) => return Err(e),
            };

            if single_ind == 0 {
                return Ok(0); // Hole
            }

            // Read third level
            match self.read_indirect_block_pointer(single_ind, third_level_idx) {
                Ok(block_ptr) => Ok(block_ptr),
                Err(FileSystemError::NotFound) => Ok(0), // Hole
                Err(e) => Err(e),
            }
        }
    }
}
