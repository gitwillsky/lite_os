//! Ordered VMA index metadata maintained in the same publication transaction as its nodes.

/// 单个 VMA 对 index metadata 的完整贡献。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct VmaContribution {
    /// VMA 当前 ordered-index key。
    pub(super) start: usize,
    /// 是否是 MemorySet 唯一 grow-down stack。
    pub(super) stack: bool,
    /// 对 RLIMIT_AS current usage 的贡献。
    pub(super) virtual_bytes: u64,
    /// 对 RLIMIT_DATA current usage 的贡献。
    pub(super) data_bytes: u64,
}

/// `MemorySet::areas` 的 O(1) identity/resource projection owner。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct VmaIndexState {
    stack_start: Option<usize>,
    virtual_bytes: u64,
    data_bytes: u64,
}

impl VmaIndexState {
    /// 构造尚未发布 VMA node 的空 index state。
    pub(super) const fn new() -> Self {
        Self {
            stack_start: None,
            virtual_bytes: 0,
            data_bytes: 0,
        }
    }

    /// 返回唯一 stack VMA 当前起始 VPN。
    pub(super) const fn stack_start(self) -> Option<usize> {
        self.stack_start
    }

    /// 返回 live user VMA 的精确 RLIMIT_AS usage。
    pub(super) const fn virtual_bytes(self) -> u64 {
        self.virtual_bytes
    }

    /// 返回 live private writable data VMA 的精确 RLIMIT_DATA usage。
    pub(super) const fn data_bytes(self) -> u64 {
        self.data_bytes
    }

    /// 在 prepared AVL node 无失败发布前登记贡献。
    ///
    /// @param contribution 与即将 commit 的唯一 node 完全对应。
    pub(super) fn publish(&mut self, contribution: VmaContribution) {
        if contribution.stack {
            assert!(
                self.stack_start.is_none(),
                "MemorySet published more than one stack VMA"
            );
            self.stack_start = Some(contribution.start);
        }
        self.virtual_bytes = self
            .virtual_bytes
            .checked_add(contribution.virtual_bytes)
            .expect("VMA virtual accounting overflow");
        self.data_bytes = self
            .data_bytes
            .checked_add(contribution.data_bytes)
            .expect("VMA data accounting overflow");
    }

    /// 在 AVL node 离开 live index 后撤销其完整贡献。
    ///
    /// @param contribution 与刚由 index 取出的唯一 node 完全对应。
    pub(super) fn retire(&mut self, contribution: VmaContribution) {
        if contribution.stack {
            assert_eq!(
                self.stack_start,
                Some(contribution.start),
                "MemorySet stack VMA key diverged from ordered index"
            );
            self.stack_start = None;
        }
        self.virtual_bytes = self
            .virtual_bytes
            .checked_sub(contribution.virtual_bytes)
            .expect("VMA virtual accounting underflow");
        self.data_bytes = self
            .data_bytes
            .checked_sub(contribution.data_bytes)
            .expect("VMA data accounting underflow");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contribution(start: usize, pages: u64, stack: bool, writable_data: bool) -> VmaContribution {
        VmaContribution {
            start,
            stack,
            virtual_bytes: pages * 4096,
            data_bytes: if writable_data { pages * 4096 } else { 0 },
        }
    }

    #[test]
    fn stack_rekey_grow_updates_key_and_virtual_total_atomically() {
        let mut state = VmaIndexState::new();
        let old = contribution(99, 1, true, false);
        let grown = contribution(92, 8, true, false);
        state.publish(old);
        state.retire(old);
        state.publish(grown);
        assert_eq!(state.stack_start(), Some(92));
        assert_eq!(state.virtual_bytes(), 8 * 4096);
    }

    #[test]
    fn split_merge_and_permission_change_preserve_exact_totals() {
        let mut state = VmaIndexState::new();
        let original = contribution(10, 6, false, true);
        state.publish(original);
        state.retire(original);
        let parts = [
            contribution(10, 2, false, true),
            contribution(12, 2, false, false),
            contribution(14, 2, false, true),
        ];
        for part in parts {
            state.publish(part);
        }
        assert_eq!(state.virtual_bytes(), 6 * 4096);
        assert_eq!(state.data_bytes(), 4 * 4096);

        for part in parts {
            state.retire(part);
        }
        let merged = contribution(10, 6, false, true);
        state.publish(merged);
        assert_eq!(state.virtual_bytes(), 6 * 4096);
        assert_eq!(state.data_bytes(), 6 * 4096);
        state.retire(merged);
        assert_eq!(state, VmaIndexState::new());
    }

    #[test]
    fn fork_exec_and_unpublished_rollback_have_independent_state() {
        let mappings = [
            contribution(4, 3, false, true),
            contribution(100, 1, true, false),
        ];
        let mut parent = VmaIndexState::new();
        let mut child = VmaIndexState::new();
        for mapping in mappings {
            parent.publish(mapping);
            child.publish(mapping);
        }
        let _prepared_but_unpublished = contribution(200, 8, false, true);
        assert_eq!(parent, child);
        assert_eq!(child.stack_start(), Some(100));
        assert_eq!(child.virtual_bytes(), 4 * 4096);
        assert_eq!(child.data_bytes(), 3 * 4096);
    }

    #[test]
    #[should_panic(expected = "more than one stack VMA")]
    fn duplicate_stack_publication_fail_stops() {
        let mut state = VmaIndexState::new();
        state.publish(contribution(100, 1, true, false));
        state.publish(contribution(200, 1, true, false));
    }
}
