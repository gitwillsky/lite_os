use super::*;

/// PONR 前解析的 canonical context storage；Arc 使注册表/reclaimer 不能退休其地址空间。
/// supervisor trap VMA 不参与用户页回收，apply 前不再取得 mm lock。
pub(super) struct PreparedContextRebind {
    address_space: Arc<AddressSpace>,
    address: usize,
    pointer: core::ptr::NonNull<UserContext>,
}

impl PreparedContextRebind {
    pub(super) fn apply(self, owner: &ContextOwner<UserContext>) {
        assert!(matches!(
            owner.binding().backing(),
            ContextBacking::AddressSpace
        ));
        // SAFETY: prepare 在 mm guard 内证明 pinned canonical mapping；本 token Arc 保活它，
        // exec caller 是唯一运行 Thread，旧/new mm 跨 commit 保活。apply 无调度/分配/锁竞争。
        unsafe { owner.rebind(self.address, self.pointer) };
        drop(self.address_space);
    }
}

impl AddressSpace {
    /// 为 Thread owner 解析一次 architecture-selected trap-context backing。
    ///
    /// # Parameters
    ///
    /// - `binding`: 已在 Thread 创建时分类的 typed address/backing 配对。
    ///
    /// # Returns
    ///
    /// 与 caller 保活的 KernelStack/AddressSpace 配对的唯一 context owner。
    pub(super) fn bind_user_context(
        &self,
        binding: ContextBinding,
    ) -> Result<ContextOwner<UserContext>, MemoryError> {
        let pointer = match binding.backing() {
            ContextBacking::KernelStack => {
                core::ptr::NonNull::new(binding.address() as *mut UserContext)
                    .ok_or(MemoryError::InvalidRange)?
            }
            ContextBacking::AddressSpace => self.user_context_pointer(binding.address())?,
        };
        // SAFETY: the caller keeps either the containing KernelStack or AddressSpace mapping live
        // until retire. ContextOwner serializes all mutable access to the selected storage.
        Ok(unsafe { ContextOwner::bind(binding.address(), pointer, binding.backing()) })
    }

    /// 在一次 mm guard 内分配并绑定新 Thread trap VMA，成功后无第二个 OOM 窗口。
    ///
    /// # Errors
    ///
    /// waiter/映射分配失败时 MemoryError；MemorySet 分配事务不留下半个 VMA。
    pub(super) fn allocate_thread_context(&self) -> Result<ContextOwner<UserContext>, MemoryError> {
        let mut memory = self
            .memory_set
            .lock()
            .map_err(|_| MemoryError::OutOfMemory)?;
        let address = memory.allocate_thread_trap_context()?;
        let pointer = Self::user_context_pointer_from(&memory, address);
        // SAFETY: 新 supervisor VMA 在同一 mm guard 内分配并解析；caller 保活 AddressSpace，
        // 尚未发布的唯一 Thread 是 context 的唯一 owner，retire 时先撤销 owner 再删除 VMA。
        Ok(unsafe { ContextOwner::bind(address, pointer, ContextBacking::AddressSpace) })
    }

    /// 在 exec PONR 前解析新映像 trap backing；注册表 callback 可合法与新 mm 竞争。
    ///
    /// # Returns
    ///
    /// kernel-stack owner 不变，返回 None；AddressSpace owner 返回锁外提交 token。
    ///
    /// # Errors
    ///
    /// mm waiter OOM 在 exec 修改任何状态前返回。
    pub(super) fn prepare_context_rebind(
        self: &Arc<Self>,
        owner: &ContextOwner<UserContext>,
        address: usize,
    ) -> Result<Option<PreparedContextRebind>, MemoryError> {
        if matches!(owner.binding().backing(), ContextBacking::KernelStack) {
            return Ok(None);
        }
        let pointer = self.user_context_pointer(address)?;
        Ok(Some(PreparedContextRebind {
            address_space: self.clone(),
            address,
            pointer,
        }))
    }

    fn user_context_pointer(
        &self,
        address: usize,
    ) -> Result<core::ptr::NonNull<UserContext>, MemoryError> {
        let memory_set = self
            .memory_set
            .lock()
            .map_err(|_| MemoryError::OutOfMemory)?;
        Ok(Self::user_context_pointer_from(&memory_set, address))
    }

    fn user_context_pointer_from(
        memory_set: &MemorySet,
        address: usize,
    ) -> core::ptr::NonNull<UserContext> {
        let ppn = memory_set.trap_context_ppn(address);
        let offset = VirtualAddress::from(address).page_offset();
        assert!(offset + core::mem::size_of::<UserContext>() <= crate::memory::PAGE_SIZE);
        // SAFETY: ppn 来自仍被 memory_set lock 保活的 trap-context mapping，offset 已验证整个
        // UserContext 位于同一 frame；缺失任一条件都会形成越界或悬空 physical pointer。
        let pointer = unsafe { ppn.as_page_mut_ptr().add(offset).cast::<UserContext>() };
        assert!(
            pointer.is_aligned(),
            "UserContext physical address is not aligned"
        );
        core::ptr::NonNull::new(pointer).expect("UserContext physical pointer must be non-null")
    }
}
