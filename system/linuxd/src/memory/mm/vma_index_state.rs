//! Ordered VMA index metadata maintained in the same publication transaction as its nodes.

/// 单个 VMA 对 index metadata 的完整贡献。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct VmaContribution {
    /// 对 RLIMIT_AS current usage 的贡献。
    pub(super) virtual_bytes: u64,
    /// 对 RLIMIT_DATA current usage 的贡献。
    pub(super) data_bytes: u64,
}

/// `MemorySet::areas` 的 O(1) resource projection owner。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct VmaIndexState {
    virtual_bytes: u64,
    data_bytes: u64,
}

impl VmaIndexState {
    /// 构造尚未发布 VMA node 的空 index state。
    pub(super) const fn new() -> Self {
        Self {
            virtual_bytes: 0,
            data_bytes: 0,
        }
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
    /// # Parameters
    ///
    /// - `contribution`: 与即将 commit 的唯一 node 完全对应。
    pub(super) fn publish(&mut self, contribution: VmaContribution) {
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
    /// # Parameters
    ///
    /// - `contribution`: 与刚由 index 取出的唯一 node 完全对应。
    pub(super) fn retire(&mut self, contribution: VmaContribution) {
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

    fn contribution(pages: u64, writable_data: bool) -> VmaContribution {
        VmaContribution {
            virtual_bytes: pages * 4096,
            data_bytes: if writable_data { pages * 4096 } else { 0 },
        }
    }

    #[test]
    fn stack_growth_updates_virtual_total_atomically() {
        let mut state = VmaIndexState::new();
        let old = contribution(1, false);
        let grown = contribution(8, false);
        state.publish(old);
        state.retire(old);
        state.publish(grown);
        assert_eq!(state.virtual_bytes(), 8 * 4096);
    }

    #[test]
    fn split_merge_and_permission_change_preserve_exact_totals() {
        let mut state = VmaIndexState::new();
        let original = contribution(6, true);
        state.publish(original);
        state.retire(original);
        let parts = [
            contribution(2, true),
            contribution(2, false),
            contribution(2, true),
        ];
        for part in parts {
            state.publish(part);
        }
        assert_eq!(state.virtual_bytes(), 6 * 4096);
        assert_eq!(state.data_bytes(), 4 * 4096);

        for part in parts {
            state.retire(part);
        }
        let merged = contribution(6, true);
        state.publish(merged);
        assert_eq!(state.virtual_bytes(), 6 * 4096);
        assert_eq!(state.data_bytes(), 6 * 4096);
        state.retire(merged);
        assert_eq!(state, VmaIndexState::new());
    }

    #[test]
    fn fork_exec_and_unpublished_rollback_have_independent_state() {
        let mappings = [contribution(3, true), contribution(1, false)];
        let mut parent = VmaIndexState::new();
        let mut child = VmaIndexState::new();
        for mapping in mappings {
            parent.publish(mapping);
            child.publish(mapping);
        }
        let _prepared_but_unpublished = contribution(8, true);
        assert_eq!(parent, child);
        assert_eq!(child.virtual_bytes(), 4 * 4096);
        assert_eq!(child.data_bytes(), 3 * 4096);
    }

    #[test]
    fn multiple_grow_down_fragments_have_independent_resource_contributions() {
        let mut state = VmaIndexState::new();
        let left = contribution(2, false);
        let right = contribution(3, false);
        state.publish(left);
        state.publish(right);
        assert_eq!(state.virtual_bytes(), 5 * 4096);
        state.retire(left);
        assert_eq!(state.virtual_bytes(), 3 * 4096);
        state.retire(right);
        assert_eq!(state, VmaIndexState::new());
    }
}
