//! 事务的元数据映像 owner；普通文件数据不进入日志暂存集合。

use super::FileSystemError;
use alloc::vec::Vec;
use fallible_tree::FallibleMap;

/// 每个物理元数据块只有一个最新映像。
#[derive(Default)]
pub(in crate::ext4) struct StagedBlocks {
    entries: FallibleMap<u64, Vec<u8>>,
}

impl StagedBlocks {
    pub(super) fn get(&self, block: u64) -> Option<&Vec<u8>> {
        self.entries.get(&block)
    }
    pub(super) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
    pub(super) fn metadata_count(&self) -> usize {
        self.entries.len()
    }
    pub(super) fn all(&self) -> impl Iterator<Item = (&u64, &Vec<u8>)> {
        self.entries.iter()
    }
    pub(super) fn metadata_from(&self, start: u64) -> impl Iterator<Item = (&u64, &Vec<u8>)> {
        self.entries.iter_from(&start)
    }

    /// 所有 admission/storage 失败都发生在旧映像改变前；覆盖不消耗新 credit。
    pub(super) fn stage(
        &mut self,
        block: u64,
        bytes: &[u8],
        capacity: usize,
    ) -> Result<(), FileSystemError> {
        if !self.entries.contains_key(&block) && self.entries.len() >= capacity {
            return Err(FileSystemError::NoSpace);
        }
        if let Some(image) = self.entries.get_mut(&block) {
            assert_eq!(image.len(), bytes.len(), "staged block size changed");
            image.copy_from_slice(bytes);
        } else {
            let mut image = Vec::new();
            image
                .try_reserve_exact(bytes.len())
                .map_err(|_| FileSystemError::OutOfMemory)?;
            image.extend_from_slice(bytes);
            let entry =
                FallibleMap::try_prepare(block, image).map_err(|_| FileSystemError::OutOfMemory)?;
            self.entries.commit_vacant(entry);
        }
        Ok(())
    }

    /// overlay 覆盖同块，重复块只计一个 journal credit。
    pub(super) fn combined_metadata_count(&self, overlay: &Self) -> usize {
        self.entries
            .iter()
            .filter(|(block, _)| !overlay.entries.contains_key(block))
            .count()
            + overlay.entries.len()
    }

    /// 无分配地移交现有节点。
    pub(super) fn merge(&mut self, mut overlay: Self) {
        while let Some((&block, _)) = overlay.entries.first_key_value() {
            let entry = overlay
                .entries
                .take_entry(&block)
                .expect("staged overlay lost block");
            match self.entries.get_mut(&block) {
                Some(existing) => *existing = entry.into_value(),
                None => self.entries.commit_vacant(entry),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn replacement_keeps_one_image_and_one_credit() {
        let mut staged = StagedBlocks::default();
        staged.stage(7, &[0; 16], 1).unwrap();
        staged.stage(7, &[0x5a; 16], 1).unwrap();
        assert_eq!(staged.metadata_count(), 1);
        assert_eq!(staged.get(7).unwrap(), &[0x5a; 16]);
    }
    #[test]
    fn failed_admission_and_cross_layer_merge_preserve_images() {
        let mut running = StagedBlocks::default();
        running.stage(7, &[1; 16], 1).unwrap();
        let mut overlay = StagedBlocks::default();
        overlay.stage(7, &[2; 16], 1).unwrap();
        assert_eq!(running.combined_metadata_count(&overlay), 1);
        running.merge(overlay);
        assert!(matches!(
            running.stage(8, &[4; 16], 1),
            Err(FileSystemError::NoSpace)
        ));
        assert_eq!(running.get(7).unwrap(), &[2; 16]);
        assert!(running.get(8).is_none());
    }
}
