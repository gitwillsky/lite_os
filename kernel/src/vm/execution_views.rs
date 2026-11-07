//! 同一 VMAR/VMO owner 的两种硬件视图：normal 可见共享服务区，private 仅可见成员私有区。
//! 私有区下级页表由 normal 唯一拥有，private 只借用根入口；supervisor 区仍分别拥有。
use core::ops::Range;

use super::address_space::{SHARED_START, ZapMode};
use super::page_table::{PagePermissions, PageTable};
use super::translation::TranslationCommit;
use super::{VmError, map_trampoline};
use crate::arch::mmu::{AddressSpaceKind, AddressSpaceToken};
use crate::memory::{PAGE_SIZE, PhysicalPageNumber, VirtualPageNumber};

/// 一次返回用户态所选择的硬件地址空间视图，不缓存执行模式。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ExecutionView {
    /// 系统服务：成员私有区与共享服务区。
    Normal,
    /// 受限代码：只暴露成员私有区；supervisor trampoline/context 仍用于进入内核。
    Private,
}

pub(super) struct ExecutionViews {
    pub(super) normal: PageTable,
    // OWNER: 受限根只拥有根页和 supervisor 子树；私有用户子树借自 normal，同事务退休后才析构。
    private: Option<PageTable>,
}

impl ExecutionViews {
    /// 创建 normal 根；仅共享进程的成员另外创建 private 根，所有失败均在硬件发布前返回。
    pub(super) fn new(member: bool) -> Result<Self, VmError> {
        fn root() -> Result<PageTable, VmError> {
            let mut table =
                PageTable::try_new(AddressSpaceKind::User).map_err(|_| VmError::NoMemory)?;
            let mut commit = TranslationCommit::new();
            let result = map_trampoline(&mut table, &mut commit);
            commit.finish_unpublished();
            result?;
            Ok(table)
        }
        // AArch64 的 trampoline 与服务区尾端共用根入口；成员 normal 从共享 owner 借用该子树，
        // 不能预建另一份。private 的 supervisor 子树仍独立，避免把服务区 USER 映射带入。
        let trampoline_branch = crate::arch::mmu::TRAMPOLINE_ADDRESS / PageTable::SUBTREE_SIZE;
        let service_branch = super::USER_END.saturating_sub(1) / PageTable::SUBTREE_SIZE;
        Ok(Self {
            normal: if member && trampoline_branch == service_branch {
                PageTable::try_new(AddressSpaceKind::User).map_err(|_| VmError::NoMemory)?
            } else {
                root()?
            },
            private: member.then(root).transpose()?,
        })
    }

    /// 根与 ASID context 始终不替换；返回 immutable token，供最终 IRQ-masked transfer 使用。
    pub(super) fn private_token(&self) -> Option<AddressSpaceToken> {
        self.private.as_ref().map(PageTable::token)
    }

    /// 观察本空间用户翻译的唯一执行视图；不把成员的 normal 镜像重复计入。
    pub(super) fn resident_table(&self) -> &PageTable {
        self.private.as_ref().unwrap_or(&self.normal)
    }

    /// 在 normal owner 安装映射，私有区同时借给 private；错误返回前仍提交已发生的退休。
    pub(super) fn install(
        &mut self,
        vpn: VirtualPageNumber,
        ppn: PhysicalPageNumber,
        permissions: PagePermissions,
    ) -> Result<TranslationCommit, VmError> {
        fn install(
            table: &mut PageTable,
            vpn: VirtualPageNumber,
            ppn: PhysicalPageNumber,
            permissions: PagePermissions,
            commit: &mut TranslationCommit,
        ) -> Result<(), VmError> {
            match table.translate(vpn) {
                Some(entry) if entry.ppn() == ppn && entry.permissions() == permissions => {
                    if entry.accessed() {
                        Ok(())
                    } else {
                        table
                            .mark_accessed(vpn, commit)
                            .map_err(|_| VmError::BadState)
                    }
                }
                Some(_) => {
                    table.unmap(vpn, commit).map_err(|_| VmError::NoMemory)?;
                    table
                        .map(vpn, ppn, permissions, commit)
                        .map_err(|_| VmError::NoMemory)
                }
                None => table
                    .map(vpn, ppn, permissions, commit)
                    .map_err(|_| VmError::NoMemory),
            }
        }
        let mut commit = TranslationCommit::new();
        let result = (|| {
            install(&mut self.normal, vpn, ppn, permissions, &mut commit)?;
            self.share_private(vpn)?;
            // 已有共享叶项也可能因负向 TLB/权限缓存再次缺页，当前 CPU 必须刷新。
            commit.record(
                vpn.as_usize(),
                super::translation::TranslationTransition::Publish,
            );
            Ok(())
        })();
        if let Err(error) = result {
            // PTE replacement 可先撤销旧叶项再遇到 OOM；不能直接丢弃 token、泄漏或提前释放旧表。
            commit
                .synchronize()
                .unwrap_or_else(|error| panic!("user translation fence failed: {error}"));
            return Err(error);
        }
        Ok(commit)
    }

    /// 仅当 `vpn` 尚无翻译时安装（fault-around）：已有翻译保持原权限，不降级可写映射。
    ///
    /// 失败时已发生的 PTE 变化仍记录在 `commit`，调用者必须提交它。
    pub(super) fn install_if_absent(
        &mut self,
        vpn: VirtualPageNumber,
        ppn: PhysicalPageNumber,
        permissions: PagePermissions,
        commit: &mut TranslationCommit,
    ) -> Result<(), VmError> {
        if self.normal.translate(vpn).is_some() {
            return Ok(());
        }
        self.normal
            .map(vpn, ppn, permissions, commit)
            .map_err(|_| VmError::NoMemory)?;
        self.share_private(vpn)?;
        Ok(())
    }

    fn share_private(&mut self, vpn: VirtualPageNumber) -> Result<(), VmError> {
        if vpn.as_usize() < SHARED_START / PAGE_SIZE
            && let Some(private) = &mut self.private
        {
            let address = vpn.as_usize() * PAGE_SIZE;
            // SAFETY: 两根同属 ExecutionViews，所有访问由同一 SpaceInner 锁串行化，
            // 退出时先退休两个根的全部 ASID，再释放 normal 子树；private 不修改用户叶项。
            unsafe {
                self.normal
                    .share_root_range(private, address..address + PAGE_SIZE)
            }
            .map_err(|_| VmError::BadState)?;
        }
        Ok(())
    }

    /// 两种视图共用一个 fence commit；现有全 ASID 的 local/remote fence 同时覆盖两个根。
    pub(super) fn zap(
        &mut self,
        range: Range<usize>,
        mode: ZapMode,
    ) -> Result<TranslationCommit, VmError> {
        let mut commit = TranslationCommit::new();
        {
            let table = &mut self.normal;
            let end = range.end.div_ceil(PAGE_SIZE);
            let mut cursor = range.start / PAGE_SIZE;
            while let Some((page, entry)) = table.next_mapped(cursor, end) {
                cursor = page + 1;
                let vpn = VirtualPageNumber::from_vpn(page);
                match mode {
                    ZapMode::Unmap => table
                        .unmap(vpn, &mut commit)
                        .map_err(|_| VmError::NoMemory)?,
                    ZapMode::RemoveWrite => {
                        let permissions = entry.permissions();
                        if permissions.contains(PagePermissions::WRITE) {
                            table
                                .set_flags(vpn, permissions - PagePermissions::WRITE, &mut commit)
                                .map_err(|_| VmError::NoMemory)?;
                        }
                    }
                }
            }
        }
        Ok(commit)
    }

    /// supervisor context 必须同时存在于两种根；caller 持有 frame，失败后撤销并完成 fence 才可释放。
    pub(super) fn map_context(
        &mut self,
        vpn: VirtualPageNumber,
        ppn: PhysicalPageNumber,
        commit: &mut TranslationCommit,
    ) -> Result<(), VmError> {
        for table in core::iter::once(&mut self.normal).chain(self.private.iter_mut()) {
            table
                .map(
                    vpn,
                    ppn,
                    PagePermissions::READ | PagePermissions::WRITE,
                    commit,
                )
                .map_err(|_| VmError::NoMemory)?;
        }
        Ok(())
    }

    /// 也用于 map_context 的部分失败清理；不存在的叶项不触发撤销。
    pub(super) fn unmap_context(&mut self, vpn: VirtualPageNumber, commit: &mut TranslationCommit) {
        for table in core::iter::once(&mut self.normal).chain(self.private.iter_mut()) {
            if table.translate(vpn).is_some() {
                table
                    .unmap(vpn, commit)
                    .expect("thread context translation disappeared");
            }
        }
    }

    /// caller 已完成全 CPU fence，随后才能退还两个 ASID 并析构页表。
    pub(super) fn retire(&mut self) {
        self.normal.release_address_space_id_after_global_fence();
        if let Some(private) = &mut self.private {
            private.release_address_space_id_after_global_fence();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_leaf_changes_follow_normal_and_shared_service_leaf_is_absent() {
        let mut views = ExecutionViews::new(true).unwrap();
        let vpn = VirtualPageNumber::from_vpn(3);
        let service = VirtualPageNumber::from_vpn(SHARED_START / PAGE_SIZE + 7);
        let permissions = PagePermissions::READ | PagePermissions::WRITE | PagePermissions::USER;
        let page = PhysicalPageNumber::from(0x1234);
        views
            .install(vpn, page, permissions)
            .unwrap()
            .synchronize()
            .unwrap();
        let mut service_owner = ExecutionViews::new(false).unwrap();
        service_owner
            .install(service, page, permissions)
            .unwrap()
            .synchronize()
            .unwrap();
        let address = service.as_usize() * PAGE_SIZE;
        // SAFETY: owner 保活至两个视图退休；本用例串行执行所有页表访问。
        unsafe {
            service_owner
                .normal
                .share_root_range(&mut views.normal, address..address + PAGE_SIZE)
        }
        .unwrap();
        assert_eq!(views.normal.translate(vpn).unwrap().ppn(), page);
        let private = views.private.as_ref().unwrap();
        assert_eq!(private.translate(vpn).unwrap().ppn(), page);
        assert!(views.normal.translate(service).is_some());
        assert!(
            private.translate(service).is_none(),
            "service image/heap must never enter restricted hardware view"
        );
        // 老化不得改权限/物理身份；重复采样为空，软件 access fault 必须重新置位，否则会无限缺页。
        let mut sampled = TranslationCommit::new();
        assert!(
            views
                .normal
                .harvest_accessed(vpn, page, &mut sampled)
                .unwrap()
        );
        assert!(
            !views
                .normal
                .harvest_accessed(vpn, page, &mut sampled)
                .unwrap()
        );
        sampled.synchronize().unwrap();
        views
            .install(vpn, page, permissions)
            .unwrap()
            .synchronize()
            .unwrap();
        let mut sampled = TranslationCommit::new();
        assert!(
            views
                .normal
                .harvest_accessed(vpn, page, &mut sampled)
                .unwrap()
        );
        sampled.synchronize().unwrap();
        views
            .zap(PAGE_SIZE..4 * PAGE_SIZE, ZapMode::RemoveWrite)
            .unwrap()
            .synchronize()
            .unwrap();
        for table in core::iter::once(&views.normal).chain(views.private.iter()) {
            assert!(
                !table
                    .translate(vpn)
                    .unwrap()
                    .permissions()
                    .contains(PagePermissions::WRITE)
            );
        }
        views
            .zap(PAGE_SIZE..4 * PAGE_SIZE, ZapMode::Unmap)
            .unwrap()
            .synchronize()
            .unwrap();
        assert!(views.normal.translate(vpn).is_none());
        assert!(views.private.as_ref().unwrap().translate(vpn).is_none());
        // 撤销必须直接对已借用的成员生效；最后一页删去后再次装入不得留下悬空根入口。
        service_owner
            .zap(address..address + PAGE_SIZE, ZapMode::Unmap)
            .unwrap()
            .synchronize()
            .unwrap();
        assert!(views.normal.translate(service).is_none());
        service_owner
            .install(service, page, permissions)
            .unwrap()
            .synchronize()
            .unwrap();
        assert_eq!(views.normal.translate(service).unwrap().ppn(), page);
        assert!(views.private.as_ref().unwrap().translate(service).is_none());
        views.retire();
        drop(views);
        service_owner.retire();
    }
}
