//! `metadata_csum` 的全部 checksum 公式；与 Linux `fs/ext4` 对应函数逐字段一致。

use super::checksum::crc32c;
use super::*;

/// superblock checksum 覆盖 `s_checksum` 之前的全部字节。
const SUPERBLOCK_CHECKSUM_OFFSET: usize = 0x3FC;
const INODE_CHECKSUM_LO_OFFSET: usize = 0x7C;
const INODE_CHECKSUM_HI_OFFSET: usize = 0x82;
const GOOD_OLD_INODE_SIZE: usize = 128;
const GROUP_DESC_CHECKSUM_OFFSET: usize = 0x1E;

impl Ext4FileSystem {
    /// Linux `ext4_superblock_csum`：以 `!0` 起始，覆盖 `s_checksum` 之前的 1020 byte。
    pub(super) fn superblock_checksum(superblock: &Ext4SuperBlock) -> u32 {
        let mut bytes = [0u8; Ext4SuperBlock::SIZE];
        superblock.encode(&mut bytes, 0);
        crc32c(!0, &bytes[..SUPERBLOCK_CHECKSUM_OFFSET])
    }

    pub(super) fn seal_superblock(superblock: &mut Ext4SuperBlock) {
        superblock.s_checksum = Self::superblock_checksum(superblock);
    }

    /// Linux `ext4_group_desc_csum`（metadata_csum 分支），取低 16 bit。
    pub(super) fn group_desc_checksum(&self, group: usize, descriptor: &Ext4GroupDesc) -> u16 {
        let mut bytes = [0u8; Ext4GroupDesc::SIZE];
        descriptor.encode(&mut bytes, 0);
        bytes[GROUP_DESC_CHECKSUM_OFFSET..GROUP_DESC_CHECKSUM_OFFSET + 2].fill(0);
        let crc = crc32c(self.checksum_seed, &(group as u32).to_le_bytes());
        crc32c(crc, &bytes) as u16
    }

    pub(super) fn seal_group_desc(&self, group: usize, descriptor: &mut Ext4GroupDesc) {
        descriptor.bg_checksum = self.group_desc_checksum(group, descriptor);
    }

    /// Linux `ext4_block_bitmap_csum_set`：覆盖 clusters_per_group / 8 byte。
    pub(super) fn block_bitmap_checksum(&self, bitmap: &[u8]) -> u32 {
        crc32c(self.checksum_seed, &bitmap[..self.blocks_per_group / 8])
    }

    /// Linux `ext4_inode_bitmap_csum_set`：覆盖 inodes_per_group / 8 byte。
    pub(super) fn inode_bitmap_checksum(&self, bitmap: &[u8]) -> u32 {
        crc32c(self.checksum_seed, &bitmap[..self.inodes_per_group / 8])
    }

    /// Linux `ei->i_csum_seed`：filesystem seed 依次混入 le32 inode number 与 generation。
    pub(super) fn inode_checksum_seed(&self, inode_num: u32, generation: u32) -> u32 {
        let crc = crc32c(self.checksum_seed, &inode_num.to_le_bytes());
        crc32c(crc, &generation.to_le_bytes())
    }

    /// Linux `ext4_inode_csum`：两个 checksum 半字按零参与计算。
    fn inode_checksum(&self, inode_num: u32, inode: &Ext4InodeDisk) -> u32 {
        let mut bytes = [0u8; Ext4InodeDisk::SIZE];
        inode.encode(&mut bytes, 0);
        bytes[INODE_CHECKSUM_LO_OFFSET..INODE_CHECKSUM_LO_OFFSET + 2].fill(0);
        let covers_hi =
            usize::from(inode.i_extra_isize) >= INODE_CHECKSUM_HI_OFFSET + 2 - GOOD_OLD_INODE_SIZE;
        if covers_hi {
            bytes[INODE_CHECKSUM_HI_OFFSET..INODE_CHECKSUM_HI_OFFSET + 2].fill(0);
        }
        crc32c(
            self.inode_checksum_seed(inode_num, inode.i_generation),
            &bytes[..self.inode_size],
        )
    }

    /// 写入前设置 inode 的 32-bit checksum。
    pub(super) fn seal_inode(&self, inode_num: u32, inode: &mut Ext4InodeDisk) {
        let checksum = self.inode_checksum(inode_num, inode);
        inode.i_checksum_lo = checksum as u16;
        inode.i_checksum_hi = (checksum >> 16) as u16;
    }

    pub(super) fn inode_checksum_valid(&self, inode_num: u32, inode: &Ext4InodeDisk) -> bool {
        let checksum = self.inode_checksum(inode_num, inode);
        inode.i_checksum_lo == checksum as u16 && inode.i_checksum_hi == (checksum >> 16) as u16
    }
}

/// Linux `ext4_extent_block_csum`：覆盖 header 与 `eh_max` 个 entry slot。
pub(super) fn extent_block_checksum(inode_seed: u32, block: &[u8], tail_offset: usize) -> u32 {
    crc32c(inode_seed, &block[..tail_offset])
}

/// Linux `ext4_dirblock_csum`：覆盖 12-byte tail 之前的 leaf 字节。
pub(super) fn directory_block_checksum(inode_seed: u32, block: &[u8], tail_offset: usize) -> u32 {
    crc32c(inode_seed, &block[..tail_offset])
}

/// Linux `ext4_dx_csum`：覆盖到最后一个有效 dx_entry，再混入 `dt_reserved` 与零 checksum。
pub(super) fn dx_checksum(inode_seed: u32, block: &[u8], used_bytes: usize, reserved: u32) -> u32 {
    let crc = crc32c(inode_seed, &block[..used_bytes]);
    let crc = crc32c(crc, &reserved.to_le_bytes());
    crc32c(crc, &[0; 4])
}

/// Linux `ext4_orphan_file_block_csum`：orphan inode seed 混入 le64 物理 block number 与 entry 数组。
pub(super) fn orphan_block_checksum(orphan_seed: u32, physical: u64, entries: &[u8]) -> u32 {
    let crc = crc32c(orphan_seed, &physical.to_le_bytes());
    crc32c(crc, entries)
}

/// Linux `ext4_xattr_block_csum`：filesystem seed 混入 le64 block number，`h_checksum` 按零参与。
pub(super) fn xattr_block_checksum(filesystem_seed: u32, block_number: u64, block: &[u8]) -> u32 {
    const CHECKSUM_OFFSET: usize = 0x10;
    let crc = crc32c(filesystem_seed, &block_number.to_le_bytes());
    let crc = crc32c(crc, &block[..CHECKSUM_OFFSET]);
    let crc = crc32c(crc, &[0; 4]);
    crc32c(crc, &block[CHECKSUM_OFFSET + 4..])
}
