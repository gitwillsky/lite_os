use super::super::*;

const DIRECT_BLOCKS: usize = 12;
const MAX_INDIRECT_DEPTH: usize = 3;

/// Allocation-free ext2 logical-block path shared by lookup and allocation.
pub(in crate::fs::ext2) struct BlockPath {
    root: usize,
    indexes: [usize; MAX_INDIRECT_DEPTH],
    depth: usize,
}

impl BlockPath {
    /// Resolve `file_block` into the inode root slot and up to three indexes of
    /// `pointers_per_block` entries. Returns `None` when the logical block is beyond ext2's
    /// triple-indirect address space or the pointer geometry is invalid.
    fn resolve(file_block: u32, pointers_per_block: usize) -> Option<Self> {
        if pointers_per_block == 0 {
            return None;
        }
        let file_block = file_block as u64;
        if file_block < DIRECT_BLOCKS as u64 {
            return Some(Self {
                root: file_block as usize,
                indexes: [0; MAX_INDIRECT_DEPTH],
                depth: 0,
            });
        }

        let count = pointers_per_block as u64;
        let mut relative = file_block - DIRECT_BLOCKS as u64;
        let single_span = count;
        let double_span = count.checked_mul(count)?;
        let triple_span = double_span.checked_mul(count)?;
        let (root, depth, indexes) = if relative < single_span {
            (12, 1, [relative as usize, 0, 0])
        } else {
            relative -= single_span;
            if relative < double_span {
                (
                    13,
                    2,
                    [(relative / count) as usize, (relative % count) as usize, 0],
                )
            } else {
                relative -= double_span;
                if relative >= triple_span {
                    return None;
                }
                (
                    14,
                    3,
                    [
                        (relative / double_span) as usize,
                        (relative / count % count) as usize,
                        (relative % count) as usize,
                    ],
                )
            }
        };
        Some(Self {
            root,
            indexes,
            depth,
        })
    }

    /// Return the owning `i_block` slot for this path.
    pub(in crate::fs::ext2) const fn root(&self) -> usize {
        self.root
    }

    /// Iterate the exact pointer indexes from the inode root toward the data block.
    pub(in crate::fs::ext2) fn indices(&self) -> impl Iterator<Item = usize> + '_ {
        self.indexes[..self.depth].iter().copied()
    }

    /// Return the number of pointer blocks traversed by this path.
    pub(in crate::fs::ext2) const fn depth(&self) -> usize {
        self.depth
    }

    /// Return whether the inode root slot points directly to data.
    pub(in crate::fs::ext2) const fn is_direct(&self) -> bool {
        self.depth == 0
    }
}

/// Immutable view of one cache-owned ext2 pointer-block image.
struct PointerBlock {
    bytes: Arc<Vec<u8>>,
}

impl PointerBlock {
    /// Decode pointer `index` from this immutable block image.
    ///
    /// Returns `InvalidFileSystem` if index arithmetic or the block bounds are invalid.
    fn pointer(&self, index: usize) -> Result<u32, FileSystemError> {
        let pointer_size = mem::size_of::<u32>();
        let offset = index
            .checked_mul(pointer_size)
            .ok_or(FileSystemError::InvalidFileSystem)?;
        let end = offset
            .checked_add(pointer_size)
            .ok_or(FileSystemError::InvalidFileSystem)?;
        let bytes = self
            .bytes
            .get(offset..end)
            .ok_or(FileSystemError::InvalidFileSystem)?;
        Ok(u32::from_le_bytes(
            bytes
                .try_into()
                .map_err(|_| FileSystemError::InvalidFileSystem)?,
        ))
    }

    /// Decode the complete pointer block for a mutation that must rewrite it.
    ///
    /// Returns `OutOfMemory` if the mutable vector cannot be reserved, or
    /// `InvalidFileSystem` if any pointer lies outside the block image.
    fn decode(&self) -> Result<Vec<u32>, FileSystemError> {
        let count = self.bytes.len() / mem::size_of::<u32>();
        let mut pointers = Vec::new();
        record_test_allocation_attempt();
        pointers
            .try_reserve_exact(count)
            .map_err(|_| FileSystemError::OutOfMemory)?;
        for index in 0..count {
            pointers.push(self.pointer(index)?);
        }
        Ok(pointers)
    }
}

impl Ext2Inode {
    /// Classify `file_block` using this filesystem's pointer geometry without allocation.
    pub(in crate::fs::ext2) fn block_path(&self, file_block: u32) -> Option<BlockPath> {
        BlockPath::resolve(file_block, self.fs.block_size / mem::size_of::<u32>())
    }

    /// Load one cache-owned pointer-block image.
    ///
    /// Propagates block I/O and metadata-cache allocation errors.
    fn pointer_block(&self, block: u32) -> Result<PointerBlock, FileSystemError> {
        Ok(PointerBlock {
            bytes: self.fs.read_metadata_block(block)?,
        })
    }

    /// Decode one complete pointer block for a mutation that must rewrite it.
    ///
    /// Propagates metadata I/O, allocation and filesystem validation errors.
    pub(in crate::fs::ext2) fn decode_pointer_block(
        &self,
        block: u32,
    ) -> Result<Vec<u32>, FileSystemError> {
        self.pointer_block(block)?.decode()
    }

    fn resolve_mapping(&self, file_block: u32) -> Result<u32, FileSystemError> {
        let Some(path) = self.block_path(file_block) else {
            return Ok(0);
        };
        let mut block = self.disk.lock().i_block[path.root()];
        for index in path.indices() {
            if block == 0 {
                return Ok(0);
            }
            block = self.pointer_block(block)?.pointer(index)?;
        }
        Ok(block)
    }

    /// Resolve an allocated logical block, returning `NotFound` for a hole.
    ///
    /// Propagates pointer metadata I/O and filesystem validation errors.
    pub(in crate::fs::ext2) fn map_block(&self, file_block: u32) -> Result<u32, FileSystemError> {
        match self.map_block_sparse(file_block)? {
            0 => Err(FileSystemError::NotFound),
            block => Ok(block),
        }
    }

    /// Resolve a logical block while representing holes and out-of-range addresses as zero.
    ///
    /// Propagates pointer metadata I/O and filesystem validation errors.
    pub(in crate::fs::ext2) fn map_block_sparse(
        &self,
        file_block: u32,
    ) -> Result<u32, FileSystemError> {
        self.resolve_mapping(file_block)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(file_block: u32) -> (usize, Vec<usize>) {
        let path = BlockPath::resolve(file_block, 4).expect("addressable block");
        (path.root(), path.indices().collect())
    }

    #[test]
    fn boundaries_share_one_direct_to_triple_classifier() {
        assert_eq!(path(0), (0, vec![]));
        assert_eq!(path(11), (11, vec![]));
        assert_eq!(path(12), (12, vec![0]));
        assert_eq!(path(15), (12, vec![3]));
        assert_eq!(path(16), (13, vec![0, 0]));
        assert_eq!(path(31), (13, vec![3, 3]));
        assert_eq!(path(32), (14, vec![0, 0, 0]));
        assert_eq!(path(95), (14, vec![3, 3, 3]));
        assert!(BlockPath::resolve(96, 4).is_none());
    }

    #[test]
    fn all_96_toy_addressable_blocks_have_one_level_shape() {
        let mut roots = [0usize; 15];
        for file_block in 0..96 {
            let path = BlockPath::resolve(file_block, 4).expect("addressable block");
            roots[path.root()] += 1;
            assert_eq!(path.indices().count(), path.depth());
        }
        assert_eq!(&roots[..12], &[1; 12]);
        assert_eq!(roots[12], 4);
        assert_eq!(roots[13], 16);
        assert_eq!(roots[14], 64);
    }
}
