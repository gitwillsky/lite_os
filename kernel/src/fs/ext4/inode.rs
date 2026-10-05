use super::extent::{ExtentTree, Mapping, empty_root};
use super::layout::INODE_BLOCK_BYTES;
use super::*;

#[path = "inode/vfs.rs"]
mod vfs;

/// ext4 纳秒时间戳：磁盘上为低 32 位秒加 `extra`（2-bit epoch + 30-bit 纳秒）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Timestamp {
    seconds: i64,
    nanoseconds: u32,
}

impl Timestamp {
    pub(super) fn now() -> Self {
        let nanoseconds = crate::timer::get_realtime_ns();
        Self {
            seconds: (nanoseconds / 1_000_000_000) as i64,
            nanoseconds: (nanoseconds % 1_000_000_000) as u32,
        }
    }

    pub(super) fn from_seconds(seconds: u64) -> Result<Self, FileSystemError> {
        Ok(Self {
            seconds: i64::try_from(seconds).map_err(|_| FileSystemError::InvalidOperation)?,
            nanoseconds: 0,
        })
    }

    /// Linux `ext4_encode_extra_time`：epoch = (sec - (s32)sec) >> 32。
    pub(super) fn encode(self) -> (u32, u32) {
        let low = self.seconds as u32;
        let epoch = ((self.seconds - i64::from(low as i32)) >> 32) as u32 & 3;
        (low, epoch | self.nanoseconds << 2)
    }

    pub(super) fn decode(low: u32, extra: u32) -> Self {
        Self {
            seconds: i64::from(low as i32) + (i64::from(extra & 3) << 32),
            nanoseconds: extra >> 2,
        }
    }

    /// VFS 只承载非负秒数；1970 之前的时间投影为 0。
    pub(super) fn seconds(self) -> u64 {
        self.seconds.max(0) as u64
    }
}

#[derive(Debug)]
pub(super) struct Ext4Inode {
    pub(super) fs: Arc<Ext4FileSystem>,
    pub(super) inode_num: u32,
    pub(super) disk: Mutex<Ext4InodeDisk>,
}

impl Ext4Inode {
    pub(super) fn load(
        fs: Arc<Ext4FileSystem>,
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
        let cache_slot = FallibleMap::<u32, Weak<Ext4Inode>>::try_reserve_node()
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

    /// 构造新 inode 的完整磁盘镜像。
    ///
    /// # Parameters
    ///
    /// - `mode`: 已编码 type 与 permission 的 `i_mode`。
    /// - `links`: 初始 link count。
    /// - `extents`: regular、directory 与 slow symlink 为 true，fast symlink 为 false。
    pub(super) fn new_disk(
        fs: &Ext4FileSystem,
        mode: u16,
        uid: u32,
        gid: u32,
        links: u16,
        extents: bool,
    ) -> Ext4InodeDisk {
        let now = Timestamp::now();
        let mut disk = Ext4InodeDisk {
            i_mode: mode,
            i_links_count: links,
            i_extra_isize: EXT4_EXTRA_ISIZE,
            i_generation: fs.new_generation(),
            ..Default::default()
        };
        disk.set_uid(uid);
        disk.set_gid(gid);
        disk.set_atime(now);
        disk.set_mtime(now);
        disk.set_ctime(now);
        disk.set_crtime(now);
        if extents {
            disk.i_flags = EXT4_EXTENTS_FL;
            disk.set_block_bytes(&empty_root());
        }
        disk
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

    /// fast symlink 把 target 存在 `i_block`，不使用 extent tree。
    pub(super) fn is_fast_symlink(disk: &Ext4InodeDisk) -> bool {
        inode_kind::from_mode(disk.i_mode) == InodeType::SymLink
            && disk.i_flags & EXT4_EXTENTS_FL == 0
    }

    /// 返回 logical block 的已初始化物理 block；hole 与 unwritten 返回 None（读为零）。
    pub(super) fn map_block_sparse(&self, file_block: u32) -> Result<Option<u64>, FileSystemError> {
        let disk = *self.disk.lock();
        let tree = ExtentTree::new(&self.fs, self.inode_num, &disk)?;
        Ok(match tree.lookup(file_block)? {
            Mapping::Mapped(extent) if !extent.unwritten => {
                Some(extent.physical + u64::from(file_block - extent.logical))
            }
            Mapping::Mapped(_) | Mapping::Hole { .. } => None,
        })
    }

    /// 返回 logical block 的已初始化物理 block；hole 返回 `NotFound`。
    pub(super) fn map_block(&self, file_block: u32) -> Result<u64, FileSystemError> {
        self.map_block_sparse(file_block)?
            .ok_or(FileSystemError::NotFound)
    }

    /// 把 extent tree 的 root 与 sector 变化合入 inode working copy。
    pub(super) fn apply_tree(
        disk: &mut Ext4InodeDisk,
        root: [u8; INODE_BLOCK_BYTES],
        sector_delta: i64,
    ) -> Result<(), FileSystemError> {
        disk.set_block_bytes(&root);
        let sectors = i64::try_from(disk.sectors())
            .ok()
            .and_then(|sectors| sectors.checked_add(sector_delta))
            .filter(|sectors| *sectors >= 0)
            .ok_or(FileSystemError::InvalidFileSystem)?;
        disk.set_sectors(sectors as u64);
        Ok(())
    }

    pub(super) fn truncate_locked(
        &self,
        mutation: &mut MutationGuard<'_>,
        size: u64,
    ) -> Result<(), FileSystemError> {
        if self.inode_type() == InodeType::Directory {
            return Err(FileSystemError::IsDirectory);
        }
        let now = Timestamp::now();
        let mut inode = mutation.inode(self)?;
        if Self::is_fast_symlink(&inode) {
            if size != 0 {
                return Err(FileSystemError::InvalidOperation);
            }
            inode.set_block_bytes(&[0; INODE_BLOCK_BYTES]);
            inode.set_size(0);
            inode.set_mtime(now);
            inode.set_ctime(now);
            return self.fs.write_inode_disk(self.inode_num, &inode);
        }
        let old_size = inode.size();
        if size < old_size {
            let block_size = self.fs.block_size as u64;
            let keep =
                u32::try_from(size.div_ceil(block_size)).map_err(|_| FileSystemError::NoSpace)?;
            drop(inode);
            // 1. 部分保留的最后一个 block 先清零尾部，避免之后扩展文件读回旧数据。
            if !size.is_multiple_of(block_size)
                && let Some(block) = self.map_block_sparse(keep - 1)?
            {
                let mut data = try_zeroed(self.fs.block_size)?;
                self.fs.read_fs_block(block, &mut data)?;
                data[(size % block_size) as usize..].fill(0);
                self.fs.write_fs_block(block, &data)?;
            }
            // 2. 释放 keep 之后的全部 extent 与空 tree 节点。
            let mut inode = mutation.inode(self)?;
            let mut tree = ExtentTree::new(&self.fs, self.inode_num, &inode)?;
            tree.remove_from(keep)?;
            Self::apply_tree(&mut inode, tree.root(), tree.sector_delta())?;
            inode.set_size(size);
            inode.set_mtime(now);
            inode.set_ctime(now);
            self.fs.write_inode_disk(self.inode_num, &inode)?;
        } else if size > old_size {
            inode.set_size(size);
            inode.set_mtime(now);
            inode.set_ctime(now);
            self.fs.write_inode_disk(self.inode_num, &inode)?;
        }
        Ok(())
    }

    /// 释放 inode 的全部存储并归还 inode number。
    ///
    /// # Parameters
    ///
    /// - `directory`: 是否归还 group 的 used directory 计数。
    pub(super) fn reclaim_locked(
        &self,
        mutation: &mut MutationGuard<'_>,
        directory: bool,
    ) -> Result<(), FileSystemError> {
        let mut disk = mutation.inode(self)?;
        if disk.i_flags & EXT4_EXTENTS_FL != 0 {
            let mut tree = ExtentTree::new(&self.fs, self.inode_num, &disk)?;
            tree.remove_from(0)?;
            Self::apply_tree(&mut disk, tree.root(), tree.sector_delta())?;
        }
        self.fs.release_xattr_block(&mut disk)?;
        // Linux `ext4_free_inode` 保留 mode/generation 并记录 dtime；bitmap 清位后 e2fsck 视其为空闲。
        disk.i_links_count = 0;
        disk.set_size(0);
        disk.i_dtime = Timestamp::now().encode().0;
        self.fs.write_inode_disk(self.inode_num, &disk)?;
        drop(disk);
        self.fs.free_inode(self.inode_num, directory)
    }
}
