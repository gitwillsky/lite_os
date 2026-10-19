use core::{mem, ptr};

use super::{
    EXT4_BLOCK_SIZE, EXT4_DESC_SIZE, EXT4_HUGE_FILE_FL, EXT4_INODE_SIZE, Ext4DirEntry2Header,
    Ext4GroupDesc, Ext4InodeDisk, Ext4SuperBlock,
};

macro_rules! disk_layout {
    ($layout:ty) => {
        impl $layout {
            pub(super) const SIZE: usize = mem::size_of::<Self>();

            /// 从磁盘字节窗口解码一个可能未对齐的 packed 值。
            pub(super) fn decode(bytes: &[u8], offset: usize) -> Option<Self> {
                let end = offset.checked_add(Self::SIZE)?;
                let source = bytes.get(offset..end)?;
                // SAFETY: `source` 覆盖完整 packed 值；read_unaligned 按值复制且不形成
                // 指向磁盘缓冲区的引用。
                Some(unsafe { ptr::read_unaligned(source.as_ptr().cast::<Self>()) })
            }

            /// 把 packed 值编码到完整磁盘字节窗口。
            pub(super) fn encode(&self, bytes: &mut [u8], offset: usize) -> bool {
                let Some(end) = offset.checked_add(Self::SIZE) else {
                    return false;
                };
                let Some(target) = bytes.get_mut(offset..end) else {
                    return false;
                };
                // SAFETY: `target` 覆盖完整 packed 值；write_unaligned 按值写入且不会
                // 读取目标缓冲区中原有的未初始化内容。
                unsafe { ptr::write_unaligned(target.as_mut_ptr().cast::<Self>(), *self) };
                true
            }
        }
    };
}

disk_layout!(Ext4SuperBlock);
disk_layout!(Ext4GroupDesc);
disk_layout!(Ext4InodeDisk);
disk_layout!(Ext4DirEntry2Header);

const _: () = assert!(Ext4SuperBlock::SIZE == 1024);
const _: () = assert!(mem::offset_of!(Ext4SuperBlock, s_checksum_seed) == 0x270);
const _: () = assert!(mem::offset_of!(Ext4SuperBlock, s_orphan_file_inum) == 0x280);
const _: () = assert!(mem::offset_of!(Ext4SuperBlock, s_checksum) == 0x3FC);
const _: () = assert!(Ext4GroupDesc::SIZE == EXT4_DESC_SIZE);
const _: () = assert!(mem::offset_of!(Ext4GroupDesc, bg_checksum) == 0x1E);
const _: () = assert!(Ext4InodeDisk::SIZE == EXT4_INODE_SIZE);
const _: () = assert!(mem::offset_of!(Ext4InodeDisk, i_checksum_lo) == 0x7C);
const _: () = assert!(mem::offset_of!(Ext4InodeDisk, i_extra_isize) == 0x80);
const _: () = assert!(mem::offset_of!(Ext4InodeDisk, i_checksum_hi) == 0x82);
const _: () = assert!(Ext4DirEntry2Header::SIZE == 8);

/// inode `i_block` 区域的字节长度：extent root 或 fast symlink payload。
pub(super) const INODE_BLOCK_BYTES: usize = mem::size_of::<[u32; 15]>();

impl Ext4InodeDisk {
    /// 以 little-endian 字节返回 `i_block`，不暴露 packed field 地址。
    pub(super) fn block_bytes(&self) -> [u8; INODE_BLOCK_BYTES] {
        let words = self.i_block;
        let mut bytes = [0u8; INODE_BLOCK_BYTES];
        for (chunk, word) in bytes.as_chunks_mut::<4>().0.iter_mut().zip(words) {
            chunk.copy_from_slice(&word.to_le_bytes());
        }
        bytes
    }

    /// 用 little-endian 字节整体替换 `i_block`。
    pub(super) fn set_block_bytes(&mut self, bytes: &[u8; INODE_BLOCK_BYTES]) {
        let mut words = [0u32; 15];
        for (word, chunk) in words.iter_mut().zip(bytes.as_chunks::<4>().0) {
            *word = u32::from_le_bytes(*chunk);
        }
        self.i_block = words;
    }

    /// 复制 fast-symlink 的 inode-inline payload。
    pub(super) fn copy_inline_symlink(&self, target: &mut [u8]) -> bool {
        let bytes = self.block_bytes();
        let Some(source) = bytes.get(..target.len()) else {
            return false;
        };
        target.copy_from_slice(source);
        true
    }

    /// 写入 fast-symlink 的 inode-inline payload。
    pub(super) fn set_inline_symlink(&mut self, target: &[u8]) -> bool {
        if target.len() >= INODE_BLOCK_BYTES {
            return false;
        }
        let mut bytes = [0u8; INODE_BLOCK_BYTES];
        bytes[..target.len()].copy_from_slice(target);
        self.set_block_bytes(&bytes);
        true
    }

    /// regular file 与 directory 都使用 64-bit `i_size_lo | i_size_high`。
    pub(super) fn size(&self) -> u64 {
        u64::from(self.i_size_lo) | u64::from(self.i_size_high) << 32
    }

    pub(super) fn set_size(&mut self, size: u64) {
        self.i_size_lo = size as u32;
        self.i_size_high = (size >> 32) as u32;
    }

    /// 48-bit `i_blocks` 换算为 512-byte sector；`HUGE_FILE_FL` inode 以 filesystem block 计数。
    pub(super) fn sectors(&self) -> u64 {
        let raw = u64::from(self.i_blocks_lo) | u64::from(self.i_blocks_high) << 32;
        if self.i_flags & EXT4_HUGE_FILE_FL != 0 {
            raw * (EXT4_BLOCK_SIZE / 512) as u64
        } else {
            raw
        }
    }

    /// 按 inode 当前计数单位写回 sector 数。
    pub(super) fn set_sectors(&mut self, sectors: u64) {
        let raw = if self.i_flags & EXT4_HUGE_FILE_FL != 0 {
            sectors / (EXT4_BLOCK_SIZE / 512) as u64
        } else {
            sectors
        };
        self.i_blocks_lo = raw as u32;
        self.i_blocks_high = (raw >> 32) as u16;
    }

    /// 48-bit external xattr block number；零表示没有 xattr block。
    pub(super) fn file_acl(&self) -> u64 {
        u64::from(self.i_file_acl_lo) | u64::from(self.i_file_acl_high) << 32
    }

    pub(super) fn set_file_acl(&mut self, block: u64) {
        self.i_file_acl_lo = block as u32;
        self.i_file_acl_high = (block >> 32) as u16;
    }
}
