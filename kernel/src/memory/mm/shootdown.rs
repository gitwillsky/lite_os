//! 页表改动的记账与 fence：撤销映射后，先让所有 CPU 忘掉旧翻译，才能释放物理页。
//!
//! # 它解决什么问题
//!
//! 每个 CPU 都有 TLB，缓存着“虚拟页 → 物理页”的翻译。改了页表，别的 CPU 的 TLB 里可能还留着旧条目。
//! 如果撤销一个映射后立刻把物理页还给分配器，另一个 CPU 就可能通过缓存的旧翻译，读写一块已经被
//! 分配给别人的内存。所以撤销必须分两步：先改页表，再等所有 CPU 确认不再使用旧翻译（TLB shootdown），
//! 之后才允许释放。
//!
//! # 核心想法：先记账，最后统一 fence
//!
//! 一次地址空间修改往往改很多个页表项。`TranslationCommit` 是这次修改的**唯一**记账令牌：
//! `PageTable::map/unmap/...` 每改一项就调用 `record`，令牌合并出“被改动的最小页区间”和“最强的
//! fence 要求”，修改全部完成后调用一次 `synchronize`。令牌带 `#[must_use]`，忘记提交会被编译器提醒。
//!
//! # 三类转换，三种代价
//!
//! 并不是每次改动都要通知其他 CPU，关键看**旧翻译留着会怎样**：
//!
//! - `Publish`（无效 → 有效）：旧条目最多让别的 CPU 多触发一次缺页，只需要本地 fence。
//! - `Relax`（权限放宽）：最多多触发一次保护异常，同样只需要本地 fence。
//! - `Revoke`（有效 → 无效、权限收紧、换成另一个物理页）：旧条目可能访问到不该访问的内存，
//!   必须让所有其他在线 CPU 完成 fence 并确认。
//!
//! 一个令牌里出现过 `Revoke`，整次提交就按 `Revoke` 处理（`FenceStrength` 取最大值）。
//!
//! # fence 的范围
//!
//! 合并后的区间不超过 `TLB_RANGE_PAGE_LIMIT`（64）页时，逐页做精确 fence；超过则只做一次全量 fence。
//! 这样稀疏的大范围撤销不会把“区间页数”变成不受控的指令循环。可执行页的发布还要同步指令缓存。
//!
//! # 为什么页表页也要保活
//!
//! 撤销时被摘下的空页表页放在 `retired_table_pages` 里，直到 fence 完成才释放。原因同上：硬件的
//! 页表遍历器可能还沿着旧的父级表项走到这些页；提前释放会让它读到被重新使用的内存。
//!
//! # 出错时宁可泄漏，也不复用
//!
//! 两条失败路径都选择**泄漏**，而不是释放：fence 失败（例如固件 IPI 出错）时，`revoke_and_commit`
//! 泄漏被保护的 owner；令牌在 fence 完成前就被丢弃时，`Drop` 泄漏保活的页表页。泄漏只浪费内存，
//! 复用则可能破坏数据。
//!
//! # 与锁的关系
//!
//! 等待其他 CPU 确认可能很久，而对方可能正关着中断自旋等某把锁，所以持有那种锁的时候不能等远端
//! fence，见 `memory/mod.rs` 里 `KERNEL_SPACE` 的说明。
//!
//! # 阅读顺序
//!
//! `TranslationTransition` → `record_range` → `plan`（如何选 fence 范围）→ `synchronize`（本地、远端、
//! 指令缓存三步）→ `Drop` 与 `revoke_and_commit`（失败路径）。

use crate::{
    fallible_tree::{FallibleMap, VacantEntry},
    memory::page_table::TableFrame,
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
    instruction_first_physical_page: usize,
    instruction_end_physical_page: usize,
    // OWNER: detached table frames remain alive until every required hardware walker fence
    // completes. Dropping them at PTE clear would let a stale parent PTE walk reused memory.
    retired_table_pages: FallibleMap<usize, TableFrame>,
}

impl TranslationCommit {
    /// 构造尚未记录 PTE mutation 的空 commit。
    pub(in crate::memory) const fn new() -> Self {
        Self {
            first_page: usize::MAX,
            end_page: 0,
            strength: FenceStrength::None,
            instruction_first_physical_page: usize::MAX,
            instruction_end_physical_page: 0,
            retired_table_pages: FallibleMap::new(),
        }
    }

    /// 为 architecture 允许保留旧 invalid/restrictive translation 的 fault 建立 local commit。
    pub(super) fn stale_fault(page: usize) -> Self {
        let mut commit = Self::new();
        commit.record(page, TranslationTransition::Relax);
        commit
    }

    /// 合并一个由 PageTable owner 判定的 leaf transition。
    ///
    /// # Parameters
    ///
    /// - `page`: 目标 virtual page number。
    /// - `transition`: invalid/valid/permission/physical identity 的语义变化。
    pub(in crate::memory) fn record(&mut self, page: usize, transition: TranslationTransition) {
        self.record_range(page, 1, transition);
    }

    /// 合并一个 contiguous leaf span；huge leaf revoke 必须覆盖完整 translation。
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

    /// 记录本次 PTE transaction 发布 executable view 的物理页范围。
    ///
    /// caller 必须在 synchronize 前完成 instruction bytes 写入。
    ///
    /// # Parameters
    ///
    /// - `first_physical_page`: 首个被写入并将变为可执行的物理页号。
    /// - `page_count`: 连续物理页数；多个记录合并成一个保守覆盖区间。
    pub(in crate::memory) fn record_instruction_publication(
        &mut self,
        first_physical_page: usize,
        page_count: usize,
    ) {
        assert_ne!(
            page_count, 0,
            "instruction publication range must not be empty"
        );
        let end_physical_page = first_physical_page
            .checked_add(page_count)
            .expect("instruction publication page range overflow");
        self.instruction_first_physical_page = self
            .instruction_first_physical_page
            .min(first_physical_page);
        self.instruction_end_physical_page =
            self.instruction_end_physical_page.max(end_physical_page);
    }

    /// 显式结束从未发布/激活的 page-table mutation，不执行 fence。
    pub(super) fn finish_unpublished(mut self) {
        self.retired_table_pages.clear();
    }

    /// 把 architecture 已摘除的空 table owners 保活到 revoke fence 完成。
    pub(in crate::memory) fn retain_table_pages(
        &mut self,
        pages: impl IntoIterator<Item = VacantEntry<usize, TableFrame>>,
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
            local_instruction_fence: self.instruction_first_physical_page != usize::MAX,
            remote_instruction_targets: if self.instruction_first_physical_page != usize::MAX {
                online_cpus.saturating_sub(1)
            } else {
                0
            },
        }
    }

    /// 提交最小 local range fence，并只为 revoke/replace 同步远端 CPU。
    ///
    /// # Returns
    ///
    /// 所有必需 target 完成 fence 后成功；firmware 失败时返回原错误。
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
            let start = self
                .instruction_first_physical_page
                .checked_mul(PAGE_SIZE)
                .expect("instruction publication start overflow");
            let size = (self.instruction_end_physical_page - self.instruction_first_physical_page)
                .checked_mul(PAGE_SIZE)
                .expect("instruction publication size overflow");
            crate::arch::instruction::publish_range(start, size);
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

/// 在 retained owner 内撤销 translations，以唯一 commit 同步后才允许释放 owner。
///
/// # Parameters
///
/// - `retained`: 必须跨 remote fence 保活的 frame/device/writer owner。
/// - `revoke`: 只修改 PTE 并把 transition 记录进给定 token。
///
/// # Returns
///
/// fence 完成后返回 owner；失败时泄漏 owner 供 caller fail-stop。
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

/// 在 address-space owner 释放 ASID/frames 前同步清空全部 CPU translation。
///
/// 这是 address-space retirement，不是 leaf mutation 的兼容路径；caller 必须在
/// 成功返回前保留完整 MemorySet owner，失败时 fail-stop 且不得复用 ASID。
///
/// # Returns
///
/// 当前 CPU 与所有其他 online/possible CPU 完成 full fence 后成功。
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
        commit.record_instruction_publication(0x81, 1);
        let plan = commit.plan(4);
        assert_eq!(plan.remote_targets, 0);
        assert!(plan.local_instruction_fence);
        assert_eq!(plan.remote_instruction_targets, 3);
        assert_eq!(commit.instruction_first_physical_page, 0x81);
        assert_eq!(commit.instruction_end_physical_page, 0x82);
    }

    #[test]
    fn executable_publication_merges_physical_ranges_without_losing_coverage() {
        let mut commit = TranslationCommit::new();
        commit.record_instruction_publication(0x91, 2);
        commit.record_instruction_publication(0x88, 1);
        assert_eq!(commit.instruction_first_physical_page, 0x88);
        assert_eq!(commit.instruction_end_physical_page, 0x93);
        assert!(commit.plan(1).local_instruction_fence);
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
