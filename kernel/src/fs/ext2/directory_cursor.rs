/// @description ext2 directory byte cookie 在单批遍历中的唯一推进 owner。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct DirectoryCursor {
    start: usize,
    published: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RecordPosition {
    Skip,
    Visit,
}

impl DirectoryCursor {
    pub(super) const fn new(start: usize, published: u64) -> Self {
        Self { start, published }
    }

    pub(super) fn first_block(self, block_size: usize) -> usize {
        self.start / block_size
    }

    /// @description 判断 record 相对初始 cookie 的位置，并向后修正落入 merged record 的 stale cookie。
    pub(super) fn locate(&mut self, absolute: usize, next: usize) -> RecordPosition {
        if next <= self.start {
            return RecordPosition::Skip;
        }
        if absolute < self.start {
            self.published = next as u64;
            return RecordPosition::Skip;
        }
        RecordPosition::Visit
    }

    pub(super) fn consume(&mut self, next: u64) {
        self.published = next;
    }

    pub(super) const fn published(self) -> u64 {
        self.published
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resumes_at_cookie_block_without_touching_preceding_blocks() {
        let cursor = DirectoryCursor::new(5 * 4096 + 128, (5 * 4096 + 128) as u64);
        assert_eq!(cursor.first_block(4096), 5);
    }

    #[test]
    fn exact_cookie_visits_next_record() {
        let mut cursor = DirectoryCursor::new(128, 128);
        assert_eq!(cursor.locate(128, 160), RecordPosition::Visit);
        cursor.consume(160);
        assert_eq!(cursor.published(), 160);
    }

    #[test]
    fn mutation_merged_record_moves_stale_cookie_forward_without_replay() {
        let mut cursor = DirectoryCursor::new(128, 128);
        assert_eq!(cursor.locate(96, 160), RecordPosition::Skip);
        assert_eq!(cursor.published(), 160);
        assert_eq!(cursor.locate(160, 192), RecordPosition::Visit);
    }
}
