//! `ext_attr`：本实现不创建 xattr，但必须正确释放已有 external xattr block 的引用。
//!
//! in-inode xattr 随 256-byte inode slot 原样保留；external block 可被多个 inode 共享，
//! 回收 inode 时按 `h_refcount` 递减，最后一个引用释放 block（Linux `ext4_xattr_release_block`）。

use super::metadata_csum::xattr_block_checksum;
use super::*;

const XATTR_MAGIC: u32 = 0xEA02_0000;
const REFCOUNT_OFFSET: usize = 4;
const BLOCKS_OFFSET: usize = 8;
const CHECKSUM_OFFSET: usize = 0x10;

fn le32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

impl Ext4FileSystem {
    /// 释放 inode 对 external xattr block 的引用，并从 `i_blocks` 扣除该 block。
    ///
    /// # Errors
    ///
    /// magic、block 数或 checksum 不符时返回 `InvalidFileSystem`；I/O 错误原样返回。
    pub(super) fn release_xattr_block(
        &self,
        disk: &mut Ext4InodeDisk,
    ) -> Result<(), FileSystemError> {
        let block = disk.file_acl();
        if block == 0 {
            return Ok(());
        }
        let mut bytes = try_zeroed(self.block_size)?;
        self.read_fs_block(block, &mut bytes)?;
        if le32(&bytes, 0) != XATTR_MAGIC
            || le32(&bytes, BLOCKS_OFFSET) != 1
            || le32(&bytes, CHECKSUM_OFFSET)
                != xattr_block_checksum(self.checksum_seed, block, &bytes)
        {
            error!("xattr block {block} invalid");
            return Err(FileSystemError::InvalidFileSystem);
        }
        let references = le32(&bytes, REFCOUNT_OFFSET);
        match references {
            0 => return Err(FileSystemError::InvalidFileSystem),
            1 => self.free_blocks(block, 1)?,
            references => {
                bytes[REFCOUNT_OFFSET..REFCOUNT_OFFSET + 4]
                    .copy_from_slice(&(references - 1).to_le_bytes());
                let checksum = xattr_block_checksum(self.checksum_seed, block, &bytes);
                bytes[CHECKSUM_OFFSET..CHECKSUM_OFFSET + 4]
                    .copy_from_slice(&checksum.to_le_bytes());
                self.write_fs_block(block, &bytes)?;
            }
        }
        disk.set_file_acl(0);
        let sectors = disk
            .sectors()
            .checked_sub((self.block_size / 512) as u64)
            .ok_or(FileSystemError::InvalidFileSystem)?;
        disk.set_sectors(sectors);
        Ok(())
    }
}
