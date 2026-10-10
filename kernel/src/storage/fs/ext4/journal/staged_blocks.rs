//! 单个 transaction 层的 home-block image owner；分类和字节始终由同一节点提交。

use super::FileSystemError;
use crate::fallible_tree::FallibleMap;
use alloc::vec::Vec;

/// image 的持久化方式：metadata 经 JBD2，普通文件数据在 commit record 前写回 home。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::fs::ext4) enum BlockKind {
    Metadata,
    Data,
}

struct StagedBlock {
    kind: BlockKind,
    bytes: Vec<u8>,
}

/// 每个物理块只有一个分类/镜像记录；初始化到 metadata 的转换不能留下旧零数据副本。
#[derive(Default)]
pub(in crate::fs::ext4) struct StagedBlocks {
    entries: FallibleMap<u64, StagedBlock>,
    // OWNER: 与 entries 同一 mutation 更新的 journal credit aggregate；所有字段 private。
    // 缺失会让每次 metadata stage 为容量检查扫描完整 ordered-data 集合。
    metadata_blocks: usize,
}

impl StagedBlocks {
    pub(super) fn get(&self, block: u64) -> Option<&Vec<u8>> {
        self.entries.get(&block).map(|image| &image.bytes)
    }
    pub(super) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
    pub(super) fn metadata_count(&self) -> usize {
        self.metadata_blocks
    }
    pub(super) fn data_count(&self) -> usize {
        self.entries.len() - self.metadata_blocks
    }
    pub(super) fn all(&self) -> impl Iterator<Item = (&u64, &Vec<u8>)> {
        self.entries
            .iter()
            .map(|(block, image)| (block, &image.bytes))
    }
    pub(super) fn data(&self) -> impl Iterator<Item = (&u64, &Vec<u8>)> {
        self.entries.iter().filter_map(|(block, image)| {
            (image.kind == BlockKind::Data).then_some((block, &image.bytes))
        })
    }
    pub(super) fn metadata_from(&self, start: u64) -> impl Iterator<Item = (&u64, &Vec<u8>)> {
        self.entries.iter_from(&start).filter_map(|(block, image)| {
            (image.kind == BlockKind::Metadata).then_some((block, &image.bytes))
        })
    }

    /// 所有 admission/credit/storage 失败都发生在旧 image/classification 改变前。
    pub(super) fn stage(
        &mut self,
        block: u64,
        bytes: &[u8],
        kind: BlockKind,
        capacity: usize,
    ) -> Result<(), FileSystemError> {
        let old_kind = self.entries.get(&block).map(|image| image.kind);
        if kind == BlockKind::Metadata
            && old_kind != Some(BlockKind::Metadata)
            && self.metadata_blocks >= capacity
        {
            return Err(FileSystemError::NoSpace);
        }
        if let Some(image) = self.entries.get_mut(&block) {
            assert_eq!(image.bytes.len(), bytes.len(), "staged block size changed");
            self.metadata_blocks -= usize::from(image.kind == BlockKind::Metadata);
            image.kind = kind;
            image.bytes.copy_from_slice(bytes);
        } else {
            let mut image = Vec::new();
            image
                .try_reserve_exact(bytes.len())
                .map_err(|_| FileSystemError::OutOfMemory)?;
            image.extend_from_slice(bytes);
            let entry = FallibleMap::try_prepare(block, StagedBlock { kind, bytes: image })
                .map_err(|_| FileSystemError::OutOfMemory)?;
            self.entries.commit_vacant(entry);
        }
        self.metadata_blocks += usize::from(kind == BlockKind::Metadata);
        Ok(())
    }

    /// overlay 最新 image/classification 覆盖 running；credit 投影在 destructive merge 前计算。
    pub(super) fn combined_metadata_count(&self, overlay: &Self) -> usize {
        self.entries
            .iter()
            .filter(|(block, image)| {
                !overlay.entries.contains_key(block) && image.kind == BlockKind::Metadata
            })
            .count()
            + overlay.metadata_blocks
    }

    /// 无分配地移交现有节点；同块跨层换型也不会遗留第二份 image。
    pub(super) fn merge(&mut self, mut overlay: Self) {
        while let Some((&block, _)) = overlay.entries.first_key_value() {
            let entry = overlay
                .entries
                .take_entry(&block)
                .expect("staged overlay lost block");
            let metadata = usize::from(entry.value().kind == BlockKind::Metadata);
            overlay.metadata_blocks -= metadata;
            let previous_metadata = self
                .entries
                .get(&block)
                .is_some_and(|image| image.kind == BlockKind::Metadata);
            match self.entries.get_mut(&block) {
                Some(existing) => *existing = entry.into_value(),
                None => self.entries.commit_vacant(entry),
            }
            self.metadata_blocks -= usize::from(previous_metadata);
            self.metadata_blocks += metadata;
        }
        debug_assert_eq!(overlay.metadata_blocks, 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn initialization_promotion_keeps_one_image_and_one_credit() {
        let mut staged = StagedBlocks::default();
        staged.stage(7, &[0; 16], BlockKind::Data, 1).unwrap();
        staged
            .stage(7, &[0x5a; 16], BlockKind::Metadata, 1)
            .unwrap();
        assert_eq!(staged.data_count(), 0);
        assert_eq!(staged.metadata_count(), 1);
        assert_eq!(staged.all().count(), 1);
        assert_eq!(staged.get(7).unwrap(), &[0x5a; 16]);
    }
    #[test]
    fn failed_promotion_and_cross_layer_merge_preserve_single_classification() {
        let mut running = StagedBlocks::default();
        running.stage(7, &[1; 16], BlockKind::Data, 1).unwrap();
        let mut overlay = StagedBlocks::default();
        overlay.stage(7, &[2; 16], BlockKind::Metadata, 1).unwrap();
        assert_eq!(running.combined_metadata_count(&overlay), 1);
        running.merge(overlay);
        assert_eq!((running.metadata_count(), running.data_count()), (1, 0));
        running.stage(8, &[3; 16], BlockKind::Data, 1).unwrap();
        assert!(matches!(
            running.stage(8, &[4; 16], BlockKind::Metadata, 1),
            Err(FileSystemError::NoSpace)
        ));
        assert_eq!(running.get(8).unwrap(), &[3; 16]);
        assert_eq!((running.metadata_count(), running.data_count()), (1, 1));
    }
}
