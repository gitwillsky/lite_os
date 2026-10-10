use super::*;

impl TaskControlBlock {
    /// 取得当前 Thread 的 context-switch 保存区锁。
    ///
    /// # Returns
    ///
    /// KernelContext mutex；raw pointer 仅可在 TCB Arc 保活期间使用。
    pub(crate) fn kernel_context(&self) -> &Mutex<KernelContext> {
        &self.execution.kernel_cx
    }

    /// 取得首次 scheduler continuation 完成后进入的 architecture trap-return。
    pub(in crate::task) fn kernel_resume_target(&self) -> crate::arch::context::KernelResume {
        self.thread().kernel_trap_return
    }

    /// 退休 Thread trap context，并删除非 canonical temporary mapping。
    pub(in crate::task) fn remove_thread_trap_context(&self) {
        let binding = self.thread().user_context.retire();
        if !binding.requires_retirement_wait(TRAP_CONTEXT) {
            return;
        }
        self.with_cleanup_memory(&self.address_space(), |memory| {
            memory.remove_thread_trap_context(binding.address());
        });
    }

    pub(super) fn replace_user_context(&self, trap_context: UserContext) {
        self.thread().user_context.replace(trap_context);
    }

    pub(super) fn snapshot_user_context_for_clone(&self) -> UserContext {
        self.thread().user_context.snapshot_for_clone()
    }

    /// 读取 syscall input registers 并原地推进 syscall instruction PC。
    ///
    /// # Returns
    ///
    /// `(number, 六个 argument register, syscall instruction PC)`；不复制其余 UserContext。
    pub(crate) fn take_syscall_request(&self) -> (usize, [usize; 6], usize) {
        self.thread().user_context.with(|context| {
            let request = context.take_syscall_request();
            (request.number(), request.arguments(), request.instruction())
        })
    }

    /// 原地发布 syscall a0 completion，不复制其余 UserContext。
    pub(crate) fn complete_syscall(&self, completion: crate::arch::context::SyscallCompletion) {
        self.thread()
            .user_context
            .with(|context| context.complete_syscall(completion));
    }

    /// user return 前唯一发布 CPU-local trap metadata。
    ///
    /// # Returns
    ///
    /// 同一 transaction 配对的 trampoline trap-context VA。
    pub(crate) fn prepare_user_return(&self, logical_cpu: usize) -> usize {
        self.thread()
            .user_context
            .with_address(|context| context.prepare_kernel_return(logical_cpu))
            .0
    }

    /// 投影当前用户 PC，不复制 UserContext。
    pub(crate) fn user_program_counter(&self) -> usize {
        self.thread()
            .user_context
            .with(|context| context.program_counter())
    }

    /// 由静态 architecture backend 检查并处理一次用户 illegal instruction。
    ///
    /// # Returns
    ///
    /// lazy architecture state 已初始化时 Retry；真正非法时返回带 fault address 的 Signal。
    pub(crate) fn handle_illegal_instruction(
        &self,
    ) -> Result<(), crate::arch::IllegalInstructionFault> {
        let probe = self
            .thread()
            .user_context
            .with(|context| context.illegal_instruction_probe());
        let result =
            crate::arch::context::inspect_illegal_instruction(probe, |address, destination| {
                let halfword: &mut [u8; 2] = destination
                    .try_into()
                    .expect("architecture decoder requests one instruction halfword");
                self.copy_instruction_halfword(address, halfword).is_ok()
            });
        self.thread()
            .user_context
            .with(|context| context.finish_illegal_instruction(result))
    }

    /// 投影当前用户 SP，不复制 UserContext。
    pub(crate) fn user_stack_pointer(&self) -> usize {
        self.thread()
            .user_context
            .with(|context| context.stack_pointer())
    }
}
