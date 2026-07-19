use super::*;

impl AddressSpace {
    /// @description 为 Thread owner 解析一次 architecture-selected trap-context backing。
    /// @param binding 已在 Thread 创建时分类的 typed address/backing 配对。
    /// @return 与 caller 保活的 KernelStack/AddressSpace 配对的唯一 context owner。
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

    /// @description exec commit 时重绑定 AddressSpace-backed owner；kernel-stack owner 不变。
    /// @param owner 当前 Thread 的唯一 context owner。
    /// @param address 新映像中的 canonical trap-context VA。
    /// @return 无返回值；mapping 缺失属于 kernel invariant failure。
    pub(super) fn rebind_user_context(&self, owner: &ContextOwner<UserContext>, address: usize) {
        if matches!(owner.binding().backing(), ContextBacking::KernelStack) {
            return;
        }
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
        let binding = self.thread.user_context.retire();
        if !binding.requires_retirement_wait(TRAP_CONTEXT) {
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
            .remove_thread_trap_context(binding.address());
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

    /// @description 由静态 architecture backend 检查并处理一次用户 illegal instruction。
    ///
    /// @return lazy architecture state 已初始化时 Retry；真正非法时返回带 fault address 的 Signal。
    pub(crate) fn handle_illegal_instruction(
        &self,
    ) -> Result<(), crate::arch::IllegalInstructionFault> {
        let probe = self
            .thread
            .user_context
            .with(|context| context.illegal_instruction_probe());
        let result =
            crate::arch::context::inspect_illegal_instruction(probe, |address, destination| {
                let halfword: &mut [u8; 2] = destination
                    .try_into()
                    .expect("architecture decoder requests one instruction halfword");
                self.copy_instruction_halfword(address, halfword).is_ok()
            });
        self.thread
            .user_context
            .with(|context| context.finish_illegal_instruction(result))
    }

    /// @description 投影当前用户 SP，不复制 UserContext。
    pub(crate) fn user_stack_pointer(&self) -> usize {
        self.thread
            .user_context
            .with(|context| context.stack_pointer())
    }
}
