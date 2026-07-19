/// @description 在保留全部 translation backing owner 时撤销 publication，并同步完成 TLB fence。
///
/// @param retained 必须跨 fence 保活的 frame、device 或 writer owner；也可以是仍拥有这些
/// owner 的独占领域引用。
/// @param revoke 只撤销 PTE/translation publication，不得释放 `retained` 中的 owner。
/// @param synchronize 在全部目标 CPU 上同步完成对应 translation fence。
/// @return fence 成功后返回原 retained state，调用者此时才可释放 owner。
/// @errors fence 失败时遗忘 retained state 并返回原错误；调用者必须随即 fail-stop，禁止
/// unwinding 释放硬件仍可能引用的 owner。
pub(super) fn revoke_and_synchronize<T, E>(
    mut retained: T,
    revoke: impl FnOnce(&mut T),
    synchronize: impl FnOnce(&mut T) -> Result<(), E>,
) -> Result<T, E> {
    revoke(&mut retained);
    match synchronize(&mut retained) {
        Ok(()) => Ok(retained),
        Err(error) => {
            // Fence 失败后，远端 CPU 仍可能通过 stale translation 访问 backing。
            // 泄漏 owner 是 fail-stop 路径上唯一安全的资源处置；正常路径不经过这里。
            core::mem::forget(retained);
            Err(error)
        }
    }
}

/// release replay 对一个已撤销 private resident 的处置。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ReclaimReleaseDecision {
    /// true 表示移除 resident owner；false 表示 target 已满足并保留 owner 供 fault 重建 PTE。
    pub(super) release: bool,
    /// 本次移除是否按 release-time owner count 释放最后一个物理 frame owner。
    pub(super) reclaimed: bool,
}

/// @description 按 release-time owner count 和 request target 决定 resident release。
/// @param reclaimed 本次 replay 已实际计入的物理页数。
/// @param target request 允许返回的最大回收页数。
/// @param release_owner_count fence 完成后、release replay 当前观察到的 Arc strong count。
/// @return target 已满足时保留 resident；否则释放，并仅将唯一 owner 计为物理回收。
pub(super) const fn reclaim_release_decision(
    reclaimed: usize,
    target: usize,
    release_owner_count: usize,
) -> ReclaimReleaseDecision {
    if reclaimed >= target {
        ReclaimReleaseDecision {
            release: false,
            reclaimed: false,
        }
    } else {
        ReclaimReleaseDecision {
            release: true,
            reclaimed: release_owner_count == 1,
        }
    }
}
