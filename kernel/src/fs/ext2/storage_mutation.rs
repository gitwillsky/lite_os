use super::*;

impl Ext2Inode {
    pub(super) fn append_bytes(&self, buf: &[u8]) -> Result<(u64, usize), FileSystemError> {
        if self.inode_type() == InodeType::Directory {
            return Err(FileSystemError::IsDirectory);
        }
        let mutation = self.fs.begin_mutation()?;
        let offset = self.size();
        let offset_usize = usize::try_from(offset).map_err(|_| FileSystemError::NoSpace)?;
        let written = self.write_at_locked(offset_usize, buf)?;
        mutation.commit()?;
        Ok((offset, written))
    }

    /// @description 为 range 中的 hole 分配清零 blocks，并在完成后提交新 i_size。
    pub(super) fn allocate_range(&self, offset: u64, length: u64) -> Result<(), FileSystemError> {
        const BLOCKS_PER_TRANSACTION: u64 = 64;
        if self.inode_type() != InodeType::File {
            return Err(FileSystemError::InvalidOperation);
        }
        let end = offset.checked_add(length).ok_or(FileSystemError::NoSpace)?;
        let block_size = self.fs.block_size as u64;
        let first = offset / block_size;
        let last = end.div_ceil(block_size);
        let mut begin = first;
        while begin < last {
            let finish = (begin + BLOCKS_PER_TRANSACTION).min(last);
            let mutation = self.fs.begin_mutation()?;
            for index in begin..finish {
                let index = u32::try_from(index).map_err(|_| FileSystemError::NoSpace)?;
                if self.map_block_sparse(index)? == 0 {
                    self.ensure_block_mapped(index)?;
                }
            }
            mutation.commit()?;
            begin = finish;
        }
        let mutation = self.fs.begin_mutation()?;
        let mut inode = self.disk.lock();
        if end > Self::disk_size(&inode) {
            Self::set_disk_size(&mut inode, end);
        }
        inode.i_mtime = Self::now();
        inode.i_ctime = inode.i_mtime;
        self.fs.write_inode_disk(self.inode_num, &inode)?;
        drop(inode);
        mutation.commit()
    }
}
