use alloc::vec::Vec;

use super::FileSystemError;

/// Journal-transaction-owned set of block groups whose allocation counters changed.
pub(super) struct AllocationDirty {
    words: Vec<u64>,
}

impl AllocationDirty {
    pub(super) fn try_new(group_count: usize) -> Result<Self, FileSystemError> {
        let count = group_count.div_ceil(u64::BITS as usize);
        let mut words = Vec::new();
        words
            .try_reserve_exact(count)
            .map_err(|_| FileSystemError::OutOfMemory)?;
        words.resize(count, 0);
        Ok(Self { words })
    }

    pub(super) const fn empty() -> Self {
        Self { words: Vec::new() }
    }

    pub(super) fn mark(&mut self, group: usize) -> Result<(), FileSystemError> {
        let word = self
            .words
            .get_mut(group / u64::BITS as usize)
            .ok_or(FileSystemError::InvalidFileSystem)?;
        *word |= 1 << (group % u64::BITS as usize);
        Ok(())
    }

    pub(super) fn is_empty(&self) -> bool {
        self.words.iter().all(|word| *word == 0)
    }

    pub(super) fn groups(&self) -> impl Iterator<Item = usize> + '_ {
        self.words
            .iter()
            .enumerate()
            .flat_map(|(word_index, word)| {
                let mut remaining = *word;
                core::iter::from_fn(move || {
                    if remaining == 0 {
                        return None;
                    }
                    let bit = remaining.trailing_zeros() as usize;
                    remaining &= remaining - 1;
                    Some(word_index * u64::BITS as usize + bit)
                })
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracks_sparse_groups_without_duplicates() {
        let mut dirty = AllocationDirty::try_new(130).unwrap();
        dirty.mark(129).unwrap();
        dirty.mark(0).unwrap();
        dirty.mark(64).unwrap();
        dirty.mark(64).unwrap();
        assert_eq!(dirty.groups().collect::<Vec<_>>(), vec![0, 64, 129]);
    }

    #[test]
    fn rejects_group_outside_reserved_topology() {
        let mut dirty = AllocationDirty::try_new(64).unwrap();
        assert_eq!(dirty.mark(64), Err(FileSystemError::InvalidFileSystem));
        assert!(dirty.is_empty());
    }
}
