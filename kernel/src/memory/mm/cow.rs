//! fork 的写时复制（COW）：fork 时只共享，第一次写时才复制。
//!
//! # 它解决什么问题
//!
//! fork 要给子进程一份与父进程相同的地址空间。真的把所有页都复制一遍既慢又浪费——子进程常常马上
//! exec，这些页立刻就被丢掉了。
//!
//! # 核心想法
//!
//! fork 时**不复制内容**：父子共享同一批物理页，两边的页表项都去掉写权限。谁先写，谁就触发缺页，
//! 到那时才为写的一方复制一页。
//!
//! # fork 时发生的事（本文件的 `try_clone_for_fork`）
//!
//! 逐个 VMA 处理：
//!
//! - 用户私有映射（匿名或私有文件映射）：子进程拿到同一批页的记录，它们共享同一个 canonical page。
//!   父进程里原本可写的页表项改成只读，子进程用只读权限映射同一个物理页。
//! - 共享匿名映射（`MAP_SHARED`）：保持可写，父子本来就应该互相看到对方的写。
//! - 设备映射、共享文件映射：各自有独立的克隆方式。
//! - 内核自己的页（非用户 VMA）：直接复制，不走 COW。
//!
//! 两个顺序约束：
//!
//! 1. 每个 VMA 先 `try_reserve_node` 预留子进程的 VMA 节点，再去改页表。否则会在父进程的页表项已经
//!    收紧、子进程的页表项已经建好之后，才发现无处发布子进程的 VMA。
//! 2. 父进程的收紧属于 `Revoke`（权限变严），必须让其他 CPU 忘掉旧的可写翻译；所有 VMA 处理完之后
//!    统一做**一次** fence，即使中途出错也要做，因为部分页表项可能已经被收紧。子进程的地址空间还没
//!    激活，不需要 fence。
//!
//! # 第一次写时发生的事（`private_fault.rs` 的 `handle_private_resident_fault`）
//!
//! 写缺页进来后分三种情况：
//!
//! 1. 页表项其实已经可写（别的 CPU 上残留的旧翻译触发的）：只做本地 fence，返回。
//! 2. 页与别人共享（不独占）：**先**分配新页并复制内容，**然后**才撤销旧映射、等所有 CPU 确认、
//!    映射新页并替换 backing。分配失败时旧映射原封不动，缺页以 OOM 结束。
//! 3. 页独占（另一方已经退出，或各自复制过了）：不需要复制，直接把写权限加回来。
//!
//! # “独占”怎么判定
//!
//! 数的是真实的私有映射记录（`PageMappingRef` 在页自己的映射集合里登记，clone 时 `inherit`，drop 时
//! `release`），而不是 `Arc` 引用计数：回收扫描等临时 pin 不能让一个本来独占的页被误判为共享而白白
//! 复制。写权限也不单独存标志，而是由这个状态推出来（见 `resident.rs` 的 `PrivateResident::permissions`）。
//!
//! # 阅读顺序
//!
//! `try_clone_for_fork` → `resident.rs`（页的记录与 `exclusive`）→ `private_fault.rs`（写缺页的三种
//! 情况）→ `shootdown.rs`（为什么撤销后要等所有 CPU）。

use super::*;

impl MapArea {
    fn try_clone_into(
        &self,
        page_table: &mut PageTable,
        commit: &mut TranslationCommit,
    ) -> Result<Self, MemoryError> {
        let mut cloned = Self {
            vpn_range: self.vpn_range.clone(),
            data_page_offset: self.data_page_offset,
            data_frames: FallibleMap::new(),
            map_type: self.map_type,
            map_permission: self.map_permission,
            global: self.global,
            kind: self.kind,
            shared_anonymous: None,
            shared_file: None,
            device: None,
            private_file: self.private_file.clone(),
            lazy_private: self.lazy_private,
        };
        if let Err(error) = cloned.map(page_table, commit) {
            // Child page table 尚未发布或激活；撤销 translation 后可直接 Drop owner，
            // 不存在 remote stale translation retire 窗口。
            let mut rollback = TranslationCommit::new();
            cloned.unmap(page_table, &mut rollback);
            rollback.finish_unpublished();
            return Err(error);
        }
        for (vpn, source) in &self.data_frames {
            Arc::get_mut(
                cloned
                    .data_frames
                    .get_mut(vpn)
                    .expect("cloned framed VMA must own every source VPN")
                    .pinned_frame_mut(),
            )
            .expect("eager-cloned system frame must be unique")
            .bytes_mut()
            .copy_from_slice(source.pinned_frame().bytes());
        }
        Ok(cloned)
    }

    fn try_clone_data_frames(
        &self,
    ) -> Result<FallibleMap<VirtualPageNumber, PrivateResident>, MemoryError> {
        let mut cloned = FallibleMap::new();
        for (&vpn, resident) in &self.data_frames {
            // truncate 已公布长度但仍在等待 parent mm 时，parent 可暂留旧 EOF resident。
            // 晚登记 child 不一定在该次扫描内；首次继承必须服从已公布的 backing EOF。
            if let Some(backing) = &self.private_file
                && !backing.faultable(vpn)?
            {
                continue;
            }
            cloned
                .try_insert(vpn, resident.clone())
                .map_err(|_| MemoryError::OutOfMemory)?;
        }
        Ok(cloned)
    }
}

fn clone_shared_file_area(
    area: &MapArea,
    page_table: &mut PageTable,
    commit: &mut TranslationCommit,
) -> Result<MapArea, MemoryError> {
    let shared = area
        .shared_file
        .as_ref()
        .expect("shared-file clone requires shared metadata");
    let mut resident = FallibleMap::new();
    let file_size = shared.mapping.size();
    let leaf_permission = shared.leaf_permission(area.map_permission);
    for (&vpn, source) in &shared.resident {
        let relative = vpn.as_usize() - area.vpn_range.start.as_usize();
        if !shared
            .pages
            .has_file_bytes(relative as u64, file_size)
            .ok_or(MemoryError::InvalidRange)?
        {
            continue;
        }
        let pin = source.pin_installed()?;
        let leaf = pin.is_some() && MapArea::has_leaf_permission(leaf_permission);
        let cloned_page = source.inherit(leaf && leaf_permission.contains(MapPermission::W));
        let prepared = resident
            .try_prepare_vacant(vpn, cloned_page)
            .map_err(|_| MemoryError::OutOfMemory)?;
        if let Some(pin) = pin.filter(|_| leaf) {
            page_table.map(vpn, pin.ppn(), leaf_permission.into(), commit)?;
        }
        resident.commit_vacant(prepared);
    }
    Ok(MapArea {
        vpn_range: area.vpn_range.clone(),
        data_page_offset: area.data_page_offset,
        data_frames: FallibleMap::new(),
        map_type: area.map_type,
        map_permission: area.map_permission,
        global: area.global,
        kind: area.kind,
        shared_anonymous: None,
        shared_file: Some(SharedFileArea {
            mapping: shared.mapping.clone(),
            pages: shared.pages,
            resident,
        }),
        device: None,
        private_file: None,
        lazy_private: false,
    })
}

impl MemorySet {
    /// 为 fork 共享用户 frame 并把可写映射转换为 COW；supervisor frame 仍独立复制。
    /// child 必须已在 live mapping/reclaim registry 登记，但尚未创建 Thread 或激活。
    /// 调用者同时独占两个 mm；root/ASID 不替换，失败由 child owner 完整 retirement。
    pub(crate) fn try_clone_for_fork(&mut self, cloned: &mut Self) -> Result<(), MemoryError> {
        assert!(
            cloned.areas.is_empty(),
            "fork child must begin with no VMA owners"
        );
        assert!(
            cloned.executable_owner.is_none(),
            "fork child executable owner already installed"
        );
        cloned.executable_owner = self.executable_owner.clone();
        cloned.code_range = self.code_range.clone();
        cloned.program_break = self.program_break;
        cloned.argument_range = self.argument_range.clone();
        cloned.map_trampoline()?;
        let page_table = &mut self.page_table;
        let mut parent_commit = TranslationCommit::new();
        let mut child_commit = TranslationCommit::new();
        let result = self.areas.try_for_each_mut(|key, area| {
            // child 只创建 calling Thread；其他 Thread 的 temporary context 不归 child MM。
            // canonical context 单独保留，task 随后写入 spawning Thread 的实际寄存器快照。
            if crate::arch::mmu::THREAD_TRAP_CONTEXT_RANGE
                .as_ref()
                .is_some_and(|range| {
                    range.contains(&usize::from(VirtualAddress::from(area.vpn_range.start)))
                })
            {
                return Ok(());
            }
            // 先预留 cloned VMA node；缺失它会在 parent COW PTE 与 child PTE
            // 已改变后才发现无法发布 child VMA owner。
            let area_slot =
                FallibleMap::try_reserve_node().map_err(|_| MemoryError::OutOfMemory)?;
            let cloned_area = if area.map_permission.contains(MapPermission::U) {
                if area.device.is_some() {
                    area.try_clone_device_into(&mut cloned.page_table, &mut child_commit)?
                } else if area.shared_file.is_some() {
                    clone_shared_file_area(area, &mut cloned.page_table, &mut child_commit)?
                } else {
                    let cloned_area = MapArea {
                        vpn_range: area.vpn_range.clone(),
                        data_page_offset: area.data_page_offset,
                        data_frames: area.try_clone_data_frames()?,
                        map_type: area.map_type,
                        map_permission: area.map_permission,
                        global: area.global,
                        kind: area.kind,
                        shared_anonymous: area.shared_anonymous.clone(),
                        shared_file: None,
                        device: None,
                        private_file: area.private_file.clone(),
                        lazy_private: area.lazy_private,
                    };
                    if MapArea::has_leaf_permission(area.map_permission) {
                        let mut flags: PagePermissions = area.map_permission.into();
                        if area.shared_anonymous.is_none() {
                            flags.remove(PagePermissions::WRITE);
                        }
                        for (&vpn, resident) in &cloned_area.data_frames {
                            let Some(frame) = resident.pin_resident()? else {
                                continue;
                            };
                            if area.map_permission.contains(MapPermission::W)
                                && area.shared_anonymous.is_none()
                                && page_table.translate(vpn).is_some()
                            {
                                page_table.set_flags(vpn, flags, &mut parent_commit)?;
                            }
                            cloned
                                .page_table
                                .map(vpn, frame.ppn(), flags, &mut child_commit)?;
                        }
                    } else {
                        for (&vpn, _) in &cloned_area.data_frames {
                            cloned.page_table.reserve(vpn)?;
                        }
                    }
                    cloned_area
                }
            } else {
                area.try_clone_into(&mut cloned.page_table, &mut child_commit)?
            };
            cloned.commit_area(area_slot.fill(*key, cloned_area));
            Ok::<(), MemoryError>(())
        });
        child_commit.finish_unpublished();
        parent_commit
            .synchronize()
            .expect("platform translation fence failed after fork COW restriction");
        result?;
        Ok(())
    }
}
