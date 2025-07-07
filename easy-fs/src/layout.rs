

const EFS_MAGIC: u32 = 0x79736165;


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
    pub fn initialize(&mut self, total_blocks: u32, inode_bitmap_blocks: u32, inode_area_blocks: u32, data_bitmap_blocks: u32, data_area_blocks: u32) {
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

