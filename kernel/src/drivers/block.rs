use core::any::Any;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockError {
    InvalidBlock,
    IoError,
    DeviceError,
    OutOfMemory,
}

pub trait BlockDevice: Send + Sync + Any {
    fn read_block(&self, block_id: usize, buf: &mut [u8]) -> Result<(), BlockError>;
    fn write_block(&self, block_id: usize, buf: &[u8]) -> Result<(), BlockError>;
    fn num_blocks(&self) -> usize;
    fn block_size(&self) -> usize;
}

pub const BLOCK_SIZE: usize = 4096;