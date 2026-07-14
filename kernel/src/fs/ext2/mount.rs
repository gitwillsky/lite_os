use super::*;

impl Ext2FileSystem {
    /// Comprehensive superblock validation
    pub(super) fn validate_superblock(
        sb: &Ext2SuperBlock,
        block_size: usize,
    ) -> Result<(), FileSystemError> {
        // Check magic number (copy to avoid unaligned access)
        let magic = sb.s_magic;
        if magic != EXT2_SUPER_MAGIC {
            error!(
                "[EXT2] Invalid magic number: 0x{:x}, expected 0x{:x}",
                magic, EXT2_SUPER_MAGIC
            );
            return Err(FileSystemError::InvalidFileSystem);
        }

        // Validate filesystem block size (1024, 2048, 4096)
        if ![1024, 2048, 4096].contains(&block_size) {
            error!("[EXT2] Unsupported block size: {}", block_size);
            return Err(FileSystemError::InvalidFileSystem);
        }

        // Validate revision level (copy to avoid unaligned access)
        let rev_level = sb.s_rev_level;
        if rev_level != 1 {
            error!("[EXT2] Unsupported revision level: {}", rev_level);
            return Err(FileSystemError::InvalidFileSystem);
        }

        // Check basic consistency
        if sb.s_inodes_count == 0 || sb.s_blocks_count == 0 {
            error!("[EXT2] Invalid superblock: zero inodes or blocks");
            return Err(FileSystemError::InvalidFileSystem);
        }

        if sb.s_free_inodes_count > sb.s_inodes_count {
            error!("[EXT2] Invalid superblock: free inodes count exceeds total");
            return Err(FileSystemError::InvalidFileSystem);
        }

        if sb.s_free_blocks_count > sb.s_blocks_count {
            error!("[EXT2] Invalid superblock: free blocks count exceeds total");
            return Err(FileSystemError::InvalidFileSystem);
        }

        // Validate inode size
        let inode_size = if rev_level == 0 {
            128
        } else {
            sb.s_inode_size as usize
        };
        if inode_size < 128 || inode_size > block_size || (inode_size & (inode_size - 1)) != 0 {
            error!("[EXT2] Invalid inode size: {}", inode_size);
            return Err(FileSystemError::InvalidFileSystem);
        }

        // Check blocks per group (copy to avoid unaligned access)
        let blocks_per_group = sb.s_blocks_per_group;
        if blocks_per_group == 0 || blocks_per_group > block_size as u32 * 8 {
            error!("[EXT2] Invalid blocks per group: {}", blocks_per_group);
            return Err(FileSystemError::InvalidFileSystem);
        }

        // Check inodes per group
        if sb.s_inodes_per_group == 0 {
            error!("[EXT2] Invalid inodes per group: 0");
            return Err(FileSystemError::InvalidFileSystem);
        }

        // Validate first data block (copy to avoid unaligned access)
        let first_data_block = sb.s_first_data_block;
        let expected_first_data_block = if block_size == 1024 { 1 } else { 0 };
        if first_data_block != expected_first_data_block {
            error!(
                "[EXT2] Unexpected first data block: {}, expected {}",
                first_data_block, expected_first_data_block
            );
            return Err(FileSystemError::InvalidFileSystem);
        }

        // Check for unsupported features
        if rev_level >= 1 {
            // Check required features - we only support basic ext2 (copy to avoid unaligned access)
            let feature_incompat = sb.s_feature_incompat;
            let unsupported_incompat = feature_incompat & !EXT2_FEATURE_INCOMPAT_SUPPORTED;
            if unsupported_incompat != 0 {
                error!(
                    "[EXT2] Unsupported incompatible features: 0x{:x}",
                    unsupported_incompat
                );
                return Err(FileSystemError::InvalidFileSystem);
            }
            if feature_incompat & EXT2_FEATURE_INCOMPAT_FILETYPE == 0 {
                error!("[EXT2] directory entries without file_type are unsupported");
                return Err(FileSystemError::InvalidFileSystem);
            }

            if sb.s_feature_compat & EXT2_FEATURE_COMPAT_HAS_JOURNAL == 0 {
                return Err(FileSystemError::InvalidFileSystem);
            }
            let unsupported_compat = sb.s_feature_compat & !EXT2_FEATURE_COMPAT_SUPPORTED;
            if unsupported_compat != 0 {
                error!(
                    "[EXT2] Unsupported compatible features: 0x{:x}",
                    unsupported_compat
                );
                return Err(FileSystemError::InvalidFileSystem);
            }

            let feature_ro_compat = sb.s_feature_ro_compat;
            let unsupported_ro = feature_ro_compat & !EXT2_FEATURE_RO_COMPAT_SUPPORTED;
            if unsupported_ro != 0 {
                return Err(FileSystemError::InvalidFileSystem);
            }
            if feature_ro_compat & EXT2_FEATURE_RO_COMPAT_LARGE_FILE == 0 {
                error!("[EXT2] revision 1 volume does not declare large_file");
                return Err(FileSystemError::InvalidFileSystem);
            }
        }

        Ok(())
    }
}
