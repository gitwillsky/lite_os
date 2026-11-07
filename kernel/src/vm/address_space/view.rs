//! 执行视图、受限地址边界和内核用户地址复制；实际读写标记访问位，统计观测不标记。

use super::*;

impl AddressSpace {
    /// 从当前地址空间复制用户内存；缺页按普通缺页处理。
    ///
    /// # Errors
    /// 任一字节不可读返回 `NotMapped`/`AccessDenied`。
    pub(crate) fn copy_from_user(
        self: &Arc<Self>,
        address: usize,
        output: &mut [u8],
    ) -> Result<(), VmError> {
        self.for_each_user_chunk(
            address,
            output.len(),
            FaultAccess::Read,
            |page, offset, range| {
                // SAFETY: `page` 是持地址空间锁期间已验证可读的用户页在 direct map 中的地址。
                let source = unsafe { core::slice::from_raw_parts(page.add(offset), range.len()) };
                output[range].copy_from_slice(source);
            },
        )
    }

    /// 向当前地址空间复制用户内存。
    ///
    /// # Errors
    /// 任一字节不可写返回 `NotMapped`/`AccessDenied`。
    pub(crate) fn copy_to_user(
        self: &Arc<Self>,
        address: usize,
        input: &[u8],
    ) -> Result<(), VmError> {
        self.for_each_user_chunk(
            address,
            input.len(),
            FaultAccess::Write,
            |page, offset, range| {
                // SAFETY: 同 `copy_from_user`；页已验证可写且写解析保证属于本映射的 VMO。
                let target =
                    unsafe { core::slice::from_raw_parts_mut(page.add(offset), range.len()) };
                target.copy_from_slice(&input[range]);
            },
        )
    }

    /// 读取一个 4 字节对齐的用户字，翻译缺失时不缺页（futex 在桶锁内比较值使用）。
    ///
    /// # Returns
    /// `Some(value)`；空间锁忙、翻译未安装或不可读时为 `None`。不等待、不分配：
    /// 调用者应释放桶锁、用普通读取等待/缺页后重试，否则 IRQ-safe 锁下会阻塞或等待 pager I/O。
    ///
    /// # Errors
    /// 地址未对齐或超出用户区间返回 `NotMapped`。
    pub(crate) fn read_resident_word(&self, address: usize) -> Result<Option<u32>, VmError> {
        if !address.is_multiple_of(4)
            || address < USER_START
            || address.checked_add(4).is_none_or(|end| end > USER_END)
        {
            return Err(VmError::NotMapped);
        }
        let owner = self
            .shared
            .as_deref()
            .filter(|owner| owner.range.contains(&address))
            .unwrap_or(self);
        let Some(mut inner) = owner.inner.try_lock() else {
            return Ok(None);
        };
        let vpn = VirtualAddress::from(address).floor();
        let Some(entry) = inner.views.normal.translate(vpn).filter(|entry| {
            entry
                .permissions()
                .contains(PagePermissions::READ | PagePermissions::USER)
        }) else {
            return Ok(None);
        };
        inner.mark_accessed(vpn, entry)?;
        let word = entry
            .ppn()
            .as_page_mut_ptr()
            .wrapping_add(address % PAGE_SIZE)
            .cast::<u32>();
        // SAFETY: 翻译在持有地址空间锁期间有效（撤销需要同一把锁），页框经 direct map 可读；
        // 地址 4 字节对齐且不跨页。用户态可并发写该字，故按原子读取。
        Ok(Some(
            unsafe { core::sync::atomic::AtomicU32::from_ptr(word) }
                .load(core::sync::atomic::Ordering::Acquire),
        ))
    }

    fn for_each_user_chunk(
        self: &Arc<Self>,
        address: usize,
        length: usize,
        access: FaultAccess,
        mut operation: impl FnMut(*mut u8, usize, Range<usize>),
    ) -> Result<(), VmError> {
        let end = address.checked_add(length).ok_or(VmError::NotMapped)?;
        if length != 0 && (address < USER_START || end > USER_END) {
            return Err(VmError::NotMapped);
        }
        let required = access.required() | PagePermissions::USER;
        let mut cursor = address;
        while cursor < end {
            let offset = cursor % PAGE_SIZE;
            let chunk = (PAGE_SIZE - offset).min(end - cursor);
            let start = cursor - address;
            let copied = {
                let owner = self
                    .shared
                    .as_deref()
                    .filter(|owner| owner.range.contains(&cursor))
                    .unwrap_or(self);
                let mut inner = owner.lock()?;
                let vpn = VirtualAddress::from(cursor).floor();
                match inner.views.normal.translate(vpn) {
                    Some(entry) if entry.permissions().contains(required) => {
                        inner.mark_accessed(vpn, entry)?;
                        operation(entry.ppn().as_page_mut_ptr(), offset, start..start + chunk);
                        true
                    }
                    _ => false,
                }
            };
            if copied {
                cursor += chunk;
            } else {
                self.fault(cursor, access)?;
            }
        }
        Ok(())
    }

    /// 返回用户态时激活的页表 token。
    pub(crate) fn token(&self, view: ExecutionView) -> AddressSpaceToken {
        match view {
            ExecutionView::Normal => self.token.clone(),
            ExecutionView::Private => self
                .private_token
                .as_ref()
                .expect("restricted mode requires a shared-process member")
                .clone(),
        }
    }

    /// 共享进程成员具有独立 private 硬件根；普通/共享模板进程不能进入受限模式。
    pub(crate) fn has_private_view(&self) -> bool {
        self.private_token.is_some()
    }

    /// 地址是否属于共享进程成员的私有区。
    pub(crate) fn is_private_address(&self, address: usize) -> bool {
        self.has_private_view() && self.range.contains(&address)
    }

    /// 地址是否属于成员可见的共享区。
    pub(crate) fn is_shared_address(&self, address: usize) -> bool {
        self.shared
            .as_ref()
            .is_some_and(|space| space.range.contains(&address))
    }

    /// 受限缺页不能解析共享区；否则隐藏区会在下一次重试前被错误补入或无限重试。
    ///
    /// # Errors
    ///
    /// 地址超出私有区，或权限、映射和 backing 无法解析这次缺页。
    pub(crate) fn fault_private(
        self: &Arc<Self>,
        address: usize,
        access: FaultAccess,
    ) -> Result<FaultSource, VmError> {
        if !self.is_private_address(address) {
            return Err(VmError::AccessDenied);
        }
        self.fault(address, access)
    }
}

impl SpaceInner {
    /// 内核实际读写用户地址也算访问；统计观测不能调用。entry 是本 guard 内取得的有效翻译。
    /// 已访问页不做原子写；仅 AF 从 0 恢复时本地刷新，不分配、不等待远端，可用于 futex 的 IRQ-safe 桶内读取。
    pub(super) fn mark_accessed(
        &mut self,
        vpn: VirtualPageNumber,
        entry: crate::vm::page_table::PageTableEntry,
    ) -> Result<(), VmError> {
        if entry.accessed() {
            return Ok(());
        }
        let mut commit = super::TranslationCommit::new();
        self.views
            .normal
            .mark_accessed(vpn, &mut commit)
            .map_err(|_| VmError::BadState)?;
        commit
            .synchronize()
            .unwrap_or_else(|error| panic!("kernel access publication failed: {error}"));
        Ok(())
    }
}
