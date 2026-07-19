use super::*;

impl AddressSpace {
    /// @description 为 Thread owner 解析一次稳定 trap-context physical mapping。
    /// @param address canonical 或按 TID 分配的 supervisor trap-context VA。
    /// @return 与本 AddressSpace 生命周期绑定的唯一 context owner。
    pub(super) fn bind_user_context(
        &self,
        address: usize,
    ) -> Result<ContextOwner<UserContext>, MemoryError> {
        let pointer = self.user_context_pointer(address)?;
        // SAFETY: AddressSpace owns the mapped frame until exec rebind or explicit retire; pointer
        // was derived under memory_set lock and ContextOwner serializes all mutable access.
        Ok(unsafe { ContextOwner::bind(address, pointer) })
    }

    /// @description exec commit 时把现有 Thread owner 重绑定到新 AddressSpace。
    /// @param owner 当前 Thread 的唯一 context owner。
    /// @param address 新映像中的 canonical trap-context VA。
    /// @return 无返回值；mapping 缺失属于 kernel invariant failure。
    pub(super) fn rebind_user_context(&self, owner: &ContextOwner<UserContext>, address: usize) {
        // exec replacement 尚未对其他 task 发布，故这不是可竞争的 lock acquisition。
        let memory_set = self
            .memory_set
            .try_lock()
            .expect("unpublished exec address space is contended");
        let pointer = Self::user_context_pointer_from(&memory_set, address);
        // SAFETY: exec caller is the sole running Thread, retains old/new AddressSpace owners across
        // commit, and publishes this binding before old mapping retirement.
        unsafe { owner.rebind(address, pointer) };
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

impl TaskControlBlock {
    /// @description 退休 Thread trap context，并删除非 canonical temporary mapping。
    pub(in crate::task) fn remove_thread_trap_context(&self) {
        let address = self.thread.user_context.retire();
        if address == TRAP_CONTEXT {
            return;
        }
        let mut wait = self
            .thread
            .memory_retirement_wait
            .lock()
            .take()
            .expect("thread memory-retirement waiter consumed twice");
        self.process
            .address_space()
            .memory_set
            .lock_prepared(&mut wait)
            .remove_thread_trap_context(address);
    }

    /// @description 返回 trampoline 使用的当前 supervisor trap-context VA。
    pub(crate) fn user_context_va(&self) -> usize {
        self.thread.user_context.address()
    }

    pub(super) fn replace_user_context(&self, trap_context: UserContext) {
        self.thread.user_context.replace(trap_context);
    }

    pub(super) fn snapshot_user_context_for_clone(&self) -> UserContext {
        self.thread.user_context.snapshot_for_clone()
    }

    /// @description 读取 syscall input registers 并原地推进 ecall PC。
    /// @return `(number, a0..a5, ecall_pc)`；不复制其余 UserContext。
    pub(crate) fn take_syscall_request(&self) -> (usize, [usize; 6], usize) {
        self.thread.user_context.with(|context| {
            let request = context.take_syscall_request();
            (request.number(), request.arguments(), request.instruction())
        })
    }

    /// @description 原地发布 syscall a0 completion，不复制其余 UserContext。
    pub(crate) fn complete_syscall(&self, completion: crate::arch::context::SyscallCompletion) {
        self.thread
            .user_context
            .with(|context| context.complete_syscall(completion));
    }

    /// @description user return 前唯一发布 CPU-local trap metadata。
    /// @return 同一 transaction 配对的 trampoline trap-context VA。
    pub(crate) fn prepare_user_return(&self, logical_cpu: usize) -> usize {
        self.thread
            .user_context
            .with_address(|context| context.prepare_kernel_return(logical_cpu))
            .0
    }

    /// @description 投影当前用户 PC，不复制 UserContext。
    pub(crate) fn user_program_counter(&self) -> usize {
        self.thread
            .user_context
            .with(|context| context.program_counter())
    }

    /// 为确认过的首次 FP 指令原地激活 architecture-owned FP context。
    pub(crate) fn activate_user_floating_point(&self) -> bool {
        self.thread
            .user_context
            .with(|context| context.activate_floating_point())
    }

    /// @description 投影当前用户 SP，不复制 UserContext。
    pub(crate) fn user_stack_pointer(&self) -> usize {
        self.thread
            .user_context
            .with(|context| context.stack_pointer())
    }
}
