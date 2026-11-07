//! 用户缺页：解析主页、安装翻译，并在同一树锁事务内顺带只读装入相邻已驻留的文件页（fault-around）。
//!
//! 安装翻译的锁序是 VMO 树锁 → 所属空间锁 → 本空间锁；`fault` 与 `install_around` 都在 VMO 解析回调里执行。

use super::*;
use crate::vm::vmo::FAULT_AROUND_PAGES;

impl AddressSpace {
    /// 处理一次用户缺页。
    ///
    /// # Errors
    /// 地址未映射返回 `NotMapped`，权限不足返回 `AccessDenied`；其余为 VMO 解析错误。
    pub(crate) fn fault(
        self: &Arc<Self>,
        address: usize,
        access: FaultAccess,
    ) -> Result<FaultSource, VmError> {
        let (owner, mapping) = self.lookup(address)?;
        if !mapping.permissions.contains(access.required()) {
            return Err(VmError::AccessDenied);
        }
        let page_address = address & !(PAGE_SIZE - 1);
        let index = mapping.vmo_page + (page_address - mapping.base) / PAGE_SIZE;
        // 读/执行缺页顺带只读装入同一对齐窗口内相邻的已驻留文件页（见 `Vmo::with_resolved_around`）；
        // 写入走正常缺页置脏，窗口按映射边界裁剪，缺失裁剪时翻译会越过映射或覆盖其他映射的页。
        let around = (access != FaultAccess::Write).then(|| {
            let window = index & !(FAULT_AROUND_PAGES - 1);
            let first = window.max(mapping.vmo_page);
            let end =
                (window + FAULT_AROUND_PAGES).min(mapping.vmo_page + mapping.size / PAGE_SIZE);
            first..end
        });
        let (mut commit, source) = mapping.vmo.with_resolved_around(
            index,
            access == FaultAccess::Write,
            around,
            |page| {
                self.install(&owner, &mapping, page_address, page)
                    .map(|commit| (commit, page.source))
            },
            |(commit, _), pages| self.install_around(&owner, &mapping, pages, commit),
        )?;
        commit
            .synchronize()
            .unwrap_or_else(|error| panic!("user translation fence failed: {error}"));
        Ok(source)
    }

    /// 把相邻已驻留文件页只读装入，合并进主缺页的 `commit`（一次 fence）。
    ///
    /// 不覆盖已有翻译；地址空间锁不可得、页表分配失败都只是放弃，主缺页已经成功。
    /// 调用者持 VMO 树锁。
    fn install_around(
        self: &Arc<Self>,
        owner: &Arc<Self>,
        mapping: &Arc<Mapping>,
        pages: &[Option<(usize, ResolvedPage)>],
        commit: &mut TranslationCommit,
    ) {
        if pages.iter().all(Option::is_none) {
            return;
        }
        let mut permissions = mapping.permissions | PagePermissions::USER;
        permissions.remove(PagePermissions::WRITE);
        let owner_guard = if Arc::ptr_eq(owner, self) {
            None
        } else {
            let Ok(guard) = owner.lock() else { return };
            Some(guard)
        };
        let Ok(mut inner) = self.lock() else { return };
        if !mapping.live.load(Ordering::Acquire) {
            return;
        }
        for &(page_index, page) in pages.iter().flatten() {
            let address = mapping.base + (page_index - mapping.vmo_page) * PAGE_SIZE;
            let vpn = VirtualAddress::from(address).floor();
            if inner
                .views
                .install_if_absent(vpn, page.ppn, permissions, commit)
                .is_err()
            {
                break;
            }
        }
        drop(inner);
        drop(owner_guard);
    }

    /// 由 VMO 解析回调调用（持树锁）：装入一条翻译。
    fn install(
        self: &Arc<Self>,
        owner: &Arc<Self>,
        mapping: &Arc<Mapping>,
        address: usize,
        page: ResolvedPage,
    ) -> Result<TranslationCommit, VmError> {
        let mut permissions = mapping.permissions | PagePermissions::USER;
        if !page.writable {
            permissions.remove(PagePermissions::WRITE);
        }
        if page.cache == CachePolicy::Device {
            permissions |= PagePermissions::DEVICE;
        }
        let owner_guard = if Arc::ptr_eq(owner, self) {
            None
        } else {
            Some(owner.lock()?)
        };
        let mut inner = self.lock()?;
        let commit = TranslationCommit::new();
        if !mapping.live.load(Ordering::Acquire) {
            drop(owner_guard);
            return Ok(commit);
        }
        let vpn = VirtualAddress::from(address).floor();
        let commit = inner.views.install(vpn, page.ppn, permissions)?;
        drop(inner);
        drop(owner_guard);
        Ok(commit)
    }
}
