/// @description 已验证 JBD2 journal 的 descriptor 与 transaction 容量事实。
///
/// 一个 descriptor 的首 tag 额外携带 UUID，后续 tag 复用 UUID；journal 的 block 0
/// 属于 superblock，另保留一个 commit block。该 immutable 值由 Journal 唯一保存，
/// 避免每次 stage 在热路径重新执行除法。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct JournalLayout {
    tags_per_descriptor: usize,
    write_capacity: usize,
}

impl JournalLayout {
    /// @description 从 journal block 数与 filesystem block size 推导精确 layout。
    /// @return 至少能容纳 descriptor、一个 data image 与 commit 时返回 layout。
    pub(super) fn new(journal_blocks: usize, block_size: usize) -> Option<Self> {
        // 12-byte header + first 8-byte tag + 16-byte UUID。
        let remaining_tag_bytes = block_size.checked_sub(36)?;
        let tags_per_descriptor = 1 + remaining_tag_bytes / 8;
        // 排除 journal superblock 与最终 commit block；每组需要一个 descriptor，
        // 其后最多 tags_per_descriptor 个 home-block image。
        let transaction_slots = journal_blocks.checked_sub(2)?;
        let group_slots = tags_per_descriptor + 1;
        let full_groups = transaction_slots / group_slots;
        let remainder = transaction_slots % group_slots;
        let write_capacity = full_groups
            .checked_mul(tags_per_descriptor)?
            .checked_add(remainder.saturating_sub(1))?;
        (write_capacity != 0).then_some(Self {
            tags_per_descriptor,
            write_capacity,
        })
    }

    pub(super) fn tags_per_descriptor(self) -> usize {
        self.tags_per_descriptor
    }

    pub(super) fn write_capacity(self) -> usize {
        self.write_capacity
    }
}
