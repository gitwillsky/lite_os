use alloc::sync::Arc;

use crate::{block_cache::get_block_cache, block_dev::BlockDevice, BLOCK_SIZE};

type BitmapBlock = [u64; 64];

const BLOCK_BITS: usize = BLOCK_SIZE * 8;

pub struct Bitmap {
    start_block_id: usize,
    blocks: usize,
}

impl Bitmap {
    pub fn new(start_block_id: usize, blocks: usize) -> Self {
        Self { start_block_id, blocks }
    }

    pub fn alloc(&self, block_dev: &Arc<dyn BlockDevice>) -> Option<usize> {
        todo!()
    }
}
