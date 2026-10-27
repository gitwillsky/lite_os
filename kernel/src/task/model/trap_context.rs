use super::*;

impl TaskControlBlock {
    /// 将实际投递的内存 fault 快照交给本 Thread 的 architecture context owner。
    ///
    /// # Parameters
    /// - `fault`: 同一次 trap 的不可变架构快照；不回读可能被迁移覆盖的 CPU 寄存器。
    ///
    /// # Returns
    /// 无返回值；由 architecture context 保存所需 metadata。
    pub(crate) fn record_user_fault(&self, fault: crate::arch::trap::UserFault) {
        self.thread()
            .user_context
            .with(|context| context.record_user_fault(fault));
    }

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

impl TaskControlBlock {
    /// 在 graph 已证明目标处于 ptrace-stop 的短事务中编解码寄存器。
    ///
    /// # Parameters
    /// - `note`: Linux ELF regset note。
    /// - `bytes`: kernel staging，不能是用户内存。
    /// - `write`: true 写入经 architecture validator 校验的状态；false 读取。
    ///
    /// # Returns
    /// 实际 transfer 字节数。
    ///
    /// # Errors
    /// 不支持的 note 或无效 register image 返回 Io/Invalid。
    pub(in crate::task) fn ptrace_registers(
        &self,
        note: usize,
        bytes: &mut [u8],
        write: bool,
    ) -> Result<usize, super::super::PtraceError> {
        if note != syscall_abi::ptrace::NT_PRSTATUS {
            return Err(super::super::PtraceError::Io);
        }
        self.thread()
            .user_context
            .with(|context| context.ptrace_registers(note, bytes, write))
            .map_err(|()| super::super::PtraceError::Invalid)
    }

    /// ATTACH 的不可屏蔽 SIGSTOP 在 relation publication 同一 graph 事务进入真实 pending。
    /// 不分配 RT record，避免建立 relation 后 OOM 导致附加永久等待。
    pub(in crate::task) fn queue_ptrace_signal(&self, signal: usize) {
        self.thread().pending_signals.lock().queue(
            signal,
            PendingSignal::kernel(),
            &mut PreparedSignalRecord::default(),
        );
    }
}

impl TaskControlBlock {
    /// tracer 在 graph 保护的 stopped relation 内移交一次性 signal-delivery 决定。
    pub(in crate::task) fn set_ptrace_resume(&self, signal: usize, info: PendingSignal) {
        assert!(
            self.thread()
                .ptrace_resume
                .lock()
                .replace((signal, info))
                .is_none()
        );
    }
    /// 恢复的 signal frame 消费唯一移交；tracer exit/SIGKILL 不伪造新的决定。
    pub(in crate::task) fn take_ptrace_resume(&self) -> Option<(usize, PendingSignal)> {
        self.thread().ptrace_resume.lock().take()
    }
}

impl TaskControlBlock {
    /// ptrace-stop 在信号 handler 选择之前展示真实 syscall restart PC 与参数。
    ///
    /// # Returns
    /// 无返回值；同一次 restart 最多重绕一次，CONT 后尊重 debugger 的寄存器改动。
    pub(in crate::task) fn prepare_ptrace_restart(&self) {
        let restart = self.thread().syscall_restart.lock().prepare_trace();
        if let Some(restart) = restart {
            self.thread().user_context.with(|context| {
                context.restart_syscall(restart.syscall_id, restart.args, restart.syscall_pc)
            });
        }
    }
}
