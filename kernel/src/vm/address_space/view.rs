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

    /// 稳定一个对齐用户字的映射，在其空间锁内执行一次原子操作。
    ///
    /// # Parameters
    /// - `address`: 4 字节对齐、可读的用户地址。
    /// - `operation`: 只执行无分配、无等待的读取/登记；可取得 futex 桶锁，不得反向取得空间锁。
    ///
    /// # Returns
    /// 返回 `operation` 的结果；地址空间 guard 保证闭包期间映射不能撤销或更换物理身份。
    ///
    /// # Errors
    /// 地址越界、权限错误、补页或空间锁准备失败。所有等待/补页均发生在调用闭包之前。
    pub(crate) fn with_resident_word<R>(
        self: &Arc<Self>,
        address: usize,
        operation: impl FnOnce(&core::sync::atomic::AtomicU32) -> R,
    ) -> Result<R, VmError> {
        if !address.is_multiple_of(4)
            || address < USER_START
            || address.checked_add(4).is_none_or(|end| end > USER_END)
        {
            return Err(VmError::NotMapped);
        }
        let owner = self.owner_of(address)?;
        loop {
            let mut inner = owner.lock()?;
            let vpn = VirtualAddress::from(address).floor();
            if let Some(entry) = inner.views.normal.translate(vpn).filter(|entry| {
                entry
                    .permissions()
                    .contains(PagePermissions::READ | PagePermissions::USER)
            }) {
                inner.mark_accessed(vpn, entry)?;
                let word = entry
                    .ppn()
                    .as_page_mut_ptr()
                    .wrapping_add(address % PAGE_SIZE)
                    .cast::<u32>();
                // SAFETY: 空间 guard 保活已验证的可读页，地址对齐且不跨页；与用户原子写使用同宽原子访问。
                let word = unsafe { core::sync::atomic::AtomicU32::from_ptr(word) };
                return Ok(operation(word));
            }
            drop(inner);
            self.fault(address, FaultAccess::Read)?;
        }
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
