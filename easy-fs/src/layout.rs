use alloc::{sync::Arc, vec::Vec};

use crate::{BLOCK_SIZE, block_cache::get_block_cache, block_dev::BlockDevice};

const EFS_MAGIC: u32 = 0x79736165;
const INODE_DIRECT_COUNT: usize = 28;
const INODE_INDIRECT1_COUNT: usize = BLOCK_SIZE / 4;
/// The upper bound of direct inode index
const DIRECT_BOUND: usize = INODE_DIRECT_COUNT;
const INDIRECT1_BOUND: usize = DIRECT_BOUND + INODE_INDIRECT1_COUNT;

type IndirectBlock = [u32; BLOCK_SIZE / 4];

/// 超级块，存放在磁盘标号为 0 的块上
#[repr(C)]
pub struct SuperBlock {
    /// 文件系统 magic number
    magic: u32,
    /// 文件系统总块数
    pub total_blocks: u32,
    pub inode_bitmap_blocks: u32,
    pub data_bitmap_blocks: u32,
    pub data_area_blocks: u32,
}

impl SuperBlock {
    pub fn initialize(
        &mut self,
        total_blocks: u32,
        inode_bitmap_blocks: u32,
        inode_area_blocks: u32,
        data_bitmap_blocks: u32,
        data_area_blocks: u32,
    ) {
        *self = Self {
            magic: EFS_MAGIC,
            total_blocks,
            inode_bitmap_blocks,
            data_bitmap_blocks,
            data_area_blocks,
        }
    }

    pub fn is_valid(&self) -> bool {
        self.magic == EFS_MAGIC
    }
}

#[repr(C)]
pub struct DiskInode {
    pub size: u32,
    pub direct: [u32; INODE_DIRECT_COUNT],
    pub indirect1: u32,
    pub indirect2: u32,
    type_: DiskInodeType,
}

#[derive(PartialEq)]
pub enum DiskInodeType {
    File,
    Directory,
}

impl DiskInode {
    pub fn initialize(&mut self, type_: DiskInodeType) {
        self.size = 0;
        self.direct.iter_mut().for_each(|v| *v = 0);
        self.indirect1 = 0;
        self.indirect2 = 0;
        self.type_ = type_;
    }

    pub fn is_dir(&self) -> bool {
        self.type_ == DiskInodeType::Directory
    }

    pub fn is_file(&self) -> bool {
        self.type_ == DiskInodeType::File
    }

    pub fn get_block_id(&self, inner_id: u32, block_device: &Arc<dyn BlockDevice>) -> u32 {
        let inner_id = inner_id as usize;
        if inner_id < INODE_DIRECT_COUNT {
            return self.direct[inner_id];
        }

        if inner_id < INODE_INDIRECT1_COUNT {
            return get_block_cache(self.indirect1 as usize, block_device.clone())
                .lock()
                .read(0, |indirect_block: &IndirectBlock| {
                    indirect_block[inner_id - INODE_DIRECT_COUNT]
                });
        }

        let last = inner_id - INDIRECT1_BOUND;
        let indirect_1 = get_block_cache(self.indirect2 as usize, block_device.clone())
            .lock()
            .read(0, |indirect_2: &IndirectBlock| {
                indirect_2[last / INODE_INDIRECT1_COUNT]
            });
        get_block_cache(indirect_1 as usize, block_device.clone())
            .lock()
            .read(0, |indirect_1: &IndirectBlock| {
                indirect_1[last % INODE_INDIRECT1_COUNT]
            })
    }

    fn _data_blocks(size: u32) -> u32 {
        (size + BLOCK_SIZE as u32 - 1) / BLOCK_SIZE as u32
    }

    /// return block number correspond to size
    pub fn data_blocks(&self) -> u32 {
        Self::_data_blocks(self.size)
    }

    /// return number of blocks needed include indirect1/2
    pub fn total_blocks(size: u32) -> u32 {
        let data_blocks = Self::_data_blocks(size) as usize;
        let mut total = data_blocks;
        // indirect 1
        if data_blocks > INODE_DIRECT_COUNT {
            total += 1
        }

        // indirect 2
        if data_blocks > INODE_INDIRECT1_COUNT {
            total += 1;
            // sub
            total +=
                (data_blocks - INDIRECT1_BOUND + INODE_INDIRECT1_COUNT - 1) / INODE_INDIRECT1_COUNT;
        }
        total as u32
    }

    pub fn blocks_num_needed(&self, new_size: u32) -> u32 {
        assert!(new_size >= self.size);
        Self::total_blocks(new_size) - Self::total_blocks(self.size)
    }

    pub fn increase_size(
        &mut self,
        new_size: u32,
        new_blocks: Vec<u32>,
        block_device: &Arc<dyn BlockDevice>,
    ) {
        todo!()
    }
}
