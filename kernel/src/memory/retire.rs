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

/// @description private reclaim round-robin walk 的 probe 位置与已提交 cursor。
///
/// revoke 扫描与 release replay 共用同一状态机，保证对同一 resident 序列得到同一 final
/// cursor。不变量：committed cursor 只推进到最后一个实际扫描页的下一位置（未扫描任何页时
/// 等于 initial）；wrap 只是 probe 回看低地址的 walk 内部行为，不得把未扫描位置提交为
/// cursor——否则 replay 走完全部 scanned 页后停在 `after(last_scanned)`，而 final 已被
/// wrap 清零，两阶段必然 diverge。
pub(super) struct PrivateReclaimWalk {
    initial: usize,
    probe: usize,
    committed: usize,
    wrapped: bool,
}

impl PrivateReclaimWalk {
    /// @description 从 persistent reclaim cursor 建立一次 walk。
    ///
    /// @param initial 本轮扫描的起始 VPN，也是 wrap 后允许扫描的下界（不含）。
    /// @return probe 与 committed 都从 initial 开始的 walk。
    pub(super) const fn new(initial: usize) -> Self {
        Self {
            initial,
            probe: initial,
            committed: initial,
            wrapped: false,
        }
    }

    /// @description 返回下一次 resident 查询的起点 VPN。
    pub(super) const fn probe(&self) -> usize {
        self.probe
    }

    /// @description probe 之后已无 resident 时把 probe 回绕到 VPN 0；已回绕过则扫描结束。
    ///
    /// @return true 表示本次调用执行了回绕、walk 继续；false 表示 walk 已完整结束。
    pub(super) fn wrap_or_finish(&mut self) -> bool {
        if self.wrapped {
            return false;
        }
        self.probe = 0;
        self.wrapped = true;
        true
    }

    /// @description 提交一个已扫描 resident 并把 probe 与 committed 推进到其下一位置。
    ///
    /// @param vpn 本次扫描的 resident VPN。
    /// @return false 表示 walk 回绕后已回到 initial 之后，该页属于上一圈，不得重复扫描，
    /// walk 结束且 cursor 不推进。
    pub(super) fn advance(&mut self, vpn: usize) -> bool {
        if self.wrapped && vpn >= self.initial {
            return false;
        }
        let next = vpn.checked_add(1).unwrap_or(0);
        self.probe = next;
        self.committed = next;
        true
    }

    /// @description 返回本 walk 提交的最终 cursor；恒等于最后一个扫描页的下一位置。
    pub(super) const fn committed(&self) -> usize {
        self.committed
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
