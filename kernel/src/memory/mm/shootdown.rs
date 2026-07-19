//! Page-table translation-change classification and fence commit owner.

use crate::{
    fallible_tree::{FallibleMap, VacantEntry},
    memory::FrameTracker,
};

const PAGE_SIZE: usize = 4096;
// Linux/riscv64 uses the same bound: beyond 64 leaf invalidations one full fence is cheaper and,
// more importantly, keeps a sparse batch's enclosing span from becoming an unbounded hot loop.
const TLB_RANGE_PAGE_LIMIT: usize = 64;

/// 单个 leaf PTE mutation 的架构语义。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::memory) enum TranslationTransition {
    /// invalid → valid leaf；旧 translation 最多造成一次额外 page fault。
    Publish,
    /// permission increase；旧 translation 最多造成一次额外 protection fault。
    Relax,
    /// valid → invalid、permission restriction 或 physical-frame replacement。
    Revoke,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum FenceStrength {
    None,
    Local,
    Remote,
}

/// 一次 MemorySet mutation 的唯一 translation-fence commit token。
///
/// token 合并全部 PTE transition 和最小覆盖页区间；caller 不得以 bool 复制 fence policy。
#[must_use = "page-table mutations must commit their translation-fence token"]
pub(in crate::memory) struct TranslationCommit {
    first_page: usize,
    end_page: usize,
    strength: FenceStrength,
    instruction_publication: bool,
    // OWNER: detached table frames remain alive until every required hardware walker fence
    // completes. Dropping them at PTE clear would let a stale parent PTE walk reused memory.
    retired_table_pages: FallibleMap<usize, FrameTracker>,
}

impl TranslationCommit {
    /// @description 构造尚未记录 PTE mutation 的空 commit。
    pub(in crate::memory) const fn new() -> Self {
        Self {
            first_page: usize::MAX,
            end_page: 0,
            strength: FenceStrength::None,
            instruction_publication: false,
            retired_table_pages: FallibleMap::new(),
        }
    }

    /// @description 为 architecture 允许保留旧 invalid/restrictive translation 的 fault 建立 local commit。
    pub(super) fn stale_fault(page: usize) -> Self {
        let mut commit = Self::new();
        commit.record(page, TranslationTransition::Relax);
        commit
    }

    /// @description 合并一个由 PageTable owner 判定的 leaf transition。
    /// @param page 目标 virtual page number。
    /// @param transition invalid/valid/permission/physical identity 的语义变化。
    pub(in crate::memory) fn record(&mut self, page: usize, transition: TranslationTransition) {
        self.record_range(page, 1, transition);
    }

    /// @description 合并一个 contiguous leaf span；huge leaf revoke 必须覆盖完整 translation。
    pub(in crate::memory) fn record_range(
        &mut self,
        first_page: usize,
        page_count: usize,
        transition: TranslationTransition,
    ) {
        assert_ne!(page_count, 0, "translation range must not be empty");
        let end_page = first_page
            .checked_add(page_count)
            .expect("translation page range overflow");
        self.first_page = self.first_page.min(first_page);
        self.end_page = self.end_page.max(end_page);
        self.strength = self.strength.max(match transition {
            TranslationTransition::Publish | TranslationTransition::Relax => FenceStrength::Local,
            TranslationTransition::Revoke => FenceStrength::Remote,
        });
    }

    /// @description 标记本次 PTE transaction 发布了新的 executable view。
    /// @note caller 必须在 synchronize 前完成 instruction bytes 写入。
    pub(in crate::memory) fn record_instruction_publication(&mut self) {
        self.instruction_publication = true;
    }

    /// @description 显式结束从未发布/激活的 page-table mutation，不执行 fence。
    pub(super) fn finish_unpublished(mut self) {
        self.retired_table_pages.clear();
    }

    /// @description 把 architecture 已摘除的空 table owners 保活到 revoke fence 完成。
    pub(in crate::memory) fn retain_table_pages(
        &mut self,
        pages: impl IntoIterator<Item = VacantEntry<usize, FrameTracker>>,
    ) {
        for page in pages {
            self.retired_table_pages.commit_vacant(page);
        }
    }

    fn plan(&self, online_cpus: usize) -> FencePlan {
        let pages = if self.strength == FenceStrength::None {
            0
        } else {
            self.end_page - self.first_page
        };
        let scope = match pages {
            0 => FenceScope::None,
            1..=TLB_RANGE_PAGE_LIMIT => FenceScope::Range {
                start: self
                    .first_page
                    .checked_mul(PAGE_SIZE)
                    .expect("translation start overflow"),
                size: pages
                    .checked_mul(PAGE_SIZE)
                    .expect("translation size overflow"),
            },
            _ => FenceScope::All,
        };
        FencePlan {
            scope,
            local_fences: match scope {
                FenceScope::None => 0,
                FenceScope::Range { .. } => pages,
                FenceScope::All => 1,
            },
            remote_targets: if self.strength == FenceStrength::Remote {
                online_cpus.saturating_sub(1)
            } else {
                0
            },
            local_instruction_fence: self.instruction_publication,
            remote_instruction_targets: if self.instruction_publication {
                online_cpus.saturating_sub(1)
            } else {
                0
            },
        }
    }

    /// @description 提交最小 local range fence，并只为 revoke/replace 同步远端 CPU。
    /// @return 所有必需 target 完成 fence 后成功；firmware 失败时返回原错误。
    #[cfg(not(test))]
    pub(super) fn synchronize(&mut self) -> Result<(), TranslationSynchronizationError> {
        let plan = self.plan(crate::cpu::online().iter().count());
        let remote_range = match plan.scope {
            FenceScope::None => None,
            FenceScope::Range { start, size } => {
                crate::arch::mmu::flush_local_range(start, size);
                Some((start, size))
            }
            FenceScope::All => {
                crate::arch::mmu::flush_local();
                Some((0, usize::MAX))
            }
        };
        if let Some((start, size)) = remote_range
            && plan.remote_targets != 0
        {
            let mut targets = crate::cpu::online() & crate::cpu::possible();
            targets.remove(crate::cpu::current_id());
            crate::platform::synchronize_tlb(targets, start, size)
                .map_err(TranslationSynchronizationError::Translation)?;
        }
        if plan.local_instruction_fence {
            crate::arch::instruction::publish_local();
            let mut targets = crate::cpu::online() & crate::cpu::possible();
            targets.remove(crate::cpu::current_id());
            crate::platform::synchronize_instruction_cache(targets)
                .map_err(TranslationSynchronizationError::Instruction)?;
        }
        self.retired_table_pages.clear();
        Ok(())
    }
}

impl Drop for TranslationCommit {
    fn drop(&mut self) {
        if !self.retired_table_pages.is_empty() {
            // Missing/failed fence must leak rather than recycle a table frame still reachable by
            // a stale hardware walk. Normal success and unpublished rollback clear this storage.
            let retained = core::mem::take(&mut self.retired_table_pages);
            core::mem::forget(retained);
            debug_assert!(
                false,
                "translation commit dropped before table retirement fence"
            );
        }
    }
}

#[derive(Debug)]
#[cfg(not(test))]
pub(super) enum TranslationSynchronizationError {
    Translation(crate::platform::TlbShootdownError),
    Instruction(crate::platform::InstructionFenceError),
}

#[cfg(not(test))]
impl core::fmt::Display for TranslationSynchronizationError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Translation(error) => error.fmt(formatter),
            Self::Instruction(error) => error.fmt(formatter),
        }
    }
}

/// Deterministic fence metric derived from the production commit token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FenceScope {
    None,
    Range { start: usize, size: usize },
    All,
}

/// Deterministic fence metric derived from the production commit token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct FencePlan {
    pub(super) scope: FenceScope,
    pub(super) local_fences: usize,
    pub(super) remote_targets: usize,
    pub(super) local_instruction_fence: bool,
    pub(super) remote_instruction_targets: usize,
}

/// @description 在 retained owner 内撤销 translations，以唯一 commit 同步后才允许释放 owner。
/// @param retained 必须跨 remote fence 保活的 frame/device/writer owner。
/// @param revoke 只修改 PTE 并把 transition 记录进给定 token。
/// @return fence 完成后返回 owner；失败时泄漏 owner 供 caller fail-stop。
#[cfg(not(test))]
pub(super) fn revoke_and_commit<T>(
    mut retained: T,
    revoke: impl FnOnce(&mut T, &mut TranslationCommit),
) -> Result<T, TranslationSynchronizationError> {
    let mut commit = TranslationCommit::new();
    revoke(&mut retained, &mut commit);
    match commit.synchronize() {
        Ok(()) => Ok(retained),
        Err(error) => {
            core::mem::forget(retained);
            Err(error)
        }
    }
}

/// @description 在 address-space owner 释放 ASID/frames 前同步清空全部 CPU translation。
/// @return 当前 CPU 与所有其他 online/possible CPU 完成 full fence 后成功。
/// @note 这是 address-space retirement，不是 leaf mutation 的兼容路径；caller 必须在
/// 成功返回前保留完整 MemorySet owner，失败时 fail-stop 且不得复用 ASID。
#[cfg(not(test))]
pub(super) fn synchronize_address_space_retirement()
-> Result<(), crate::platform::TlbShootdownError> {
    crate::arch::mmu::flush_local();
    let mut targets = crate::cpu::online() & crate::cpu::possible();
    targets.remove(crate::cpu::current_id());
    crate::platform::synchronize_tlb(targets, 0, 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lazy_mapping_has_no_fence() {
        let commit = TranslationCommit::new();
        assert_eq!(commit.plan(8).remote_targets, 0);
        assert_eq!(commit.plan(8).local_fences, 0);
        commit.finish_unpublished();
    }

    #[test]
    fn one_megabyte_first_touch_has_zero_remote_targets() {
        let mut total = 0;
        for page in 0..256 {
            let mut commit = TranslationCommit::new();
            commit.record(page, TranslationTransition::Publish);
            total += commit.plan(8).remote_targets;
        }
        assert_eq!(total, 0);
    }

    #[test]
    fn executable_publication_separates_tlb_and_instruction_targets() {
        let mut commit = TranslationCommit::new();
        commit.record(3, TranslationTransition::Publish);
        commit.record_instruction_publication();
        let plan = commit.plan(4);
        assert_eq!(plan.remote_targets, 0);
        assert!(plan.local_instruction_fence);
        assert_eq!(plan.remote_instruction_targets, 3);
    }

    #[test]
    fn revoke_and_replacement_target_every_other_online_cpu() {
        let mut commit = TranslationCommit::new();
        commit.record(7, TranslationTransition::Revoke);
        let plan = commit.plan(8);
        assert_eq!(
            plan.scope,
            FenceScope::Range {
                start: 7 * PAGE_SIZE,
                size: PAGE_SIZE,
            }
        );
        assert_eq!(plan.remote_targets, 7);
    }

    #[test]
    fn huge_leaf_revoke_fences_the_complete_span() {
        let mut commit = TranslationCommit::new();
        commit.record_range(512, 512, TranslationTransition::Revoke);
        let plan = commit.plan(4);
        assert_eq!(plan.scope, FenceScope::All);
        assert_eq!(plan.local_fences, 1);
        assert_eq!(plan.remote_targets, 3);
    }

    #[test]
    fn range_fence_cost_is_bounded_at_sixty_four_pages() {
        let mut exact = TranslationCommit::new();
        exact.record_range(8, TLB_RANGE_PAGE_LIMIT, TranslationTransition::Revoke);
        assert_eq!(
            exact.plan(4).scope,
            FenceScope::Range {
                start: 8 * PAGE_SIZE,
                size: TLB_RANGE_PAGE_LIMIT * PAGE_SIZE,
            }
        );
        assert_eq!(exact.plan(4).local_fences, TLB_RANGE_PAGE_LIMIT);

        let mut over_limit = TranslationCommit::new();
        over_limit.record_range(8, TLB_RANGE_PAGE_LIMIT + 1, TranslationTransition::Revoke);
        assert_eq!(over_limit.plan(4).scope, FenceScope::All);
        assert_eq!(over_limit.plan(4).local_fences, 1);
    }

    #[test]
    fn sparse_userspace_teardown_has_constant_fence_cost() {
        let mut commit = TranslationCommit::new();
        commit.record(0x61, TranslationTransition::Revoke);
        commit.record(
            (1usize << 38) / PAGE_SIZE - 1,
            TranslationTransition::Revoke,
        );
        let plan = commit.plan(4);
        assert_eq!(plan.scope, FenceScope::All);
        assert_eq!(plan.local_fences, 1);
        assert_eq!(plan.remote_targets, 3);
    }

    #[test]
    fn permission_relax_is_local_but_restriction_dominates_batch() {
        let mut commit = TranslationCommit::stale_fault(10);
        assert_eq!(commit.plan(4).remote_targets, 0);
        commit.record(12, TranslationTransition::Revoke);
        assert_eq!(
            commit.plan(4),
            FencePlan {
                scope: FenceScope::Range {
                    start: 10 * PAGE_SIZE,
                    size: 3 * PAGE_SIZE,
                },
                local_fences: 3,
                remote_targets: 3,
                local_instruction_fence: false,
                remote_instruction_targets: 0,
            }
        );
    }
}
