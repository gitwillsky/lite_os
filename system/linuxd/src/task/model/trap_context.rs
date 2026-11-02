use super::*;

impl TaskControlBlock {
    /// 将实际投递的内存 fault 快照交给本 Thread 的寄存器上下文（信号帧的 fault 元数据）。
    pub(crate) fn record_user_fault(&self, fault: crate::arch::context::UserFault) {
        self.thread()
            .user_context
            .with(|context| context.record_user_fault(fault));
    }

    pub(super) fn replace_user_context(&self, trap_context: UserContext) {
        self.thread().user_context.replace(trap_context);
    }

    pub(super) fn snapshot_user_context_for_clone(&self) -> UserContext {
        self.thread().user_context.snapshot_for_clone()
    }

    /// 首次进入前需要装入的浮点状态（clone 继承或 exec 清零）。
    pub(crate) fn take_initial_fp(&self) -> Option<crate::arch::fp::FpState> {
        self.thread().user_context.take_initial_fp()
    }

    /// 安装承载线程绑定的 restricted 状态；替换掉的旧映射（exec 前的承载线程）随之解除。
    pub(crate) fn install_restricted_state(&self, state: native_runtime::restricted::BoundState) {
        let previous = self.thread().restricted.lock().replace(state);
        drop(previous);
    }

    /// 以当前寄存器进入 restricted mode，返回后寄存器已更新为退出时的值。
    ///
    /// # Returns
    ///
    /// 退出原因；原因为异常时附带异常报告。
    ///
    /// # Errors
    ///
    /// 内核拒绝进入（寄存器非法或状态未绑定）。
    pub(crate) fn enter_restricted(
        &self,
    ) -> Result<
        (
            native_runtime::restricted::Exit,
            native_runtime::native_abi::ExceptionReport,
        ),
        native_runtime::native_abi::Status,
    > {
        let mut slot = self.thread().restricted.lock();
        let state = slot.as_mut().expect("restricted state not installed");
        state.state().registers = self
            .thread()
            .user_context
            .with(|context| context.registers());
        let exit = state.enter()?;
        let restricted = *state.state();
        self.thread()
            .user_context
            .with(|context| context.load_registers(&restricted.registers));
        Ok((exit, restricted.exception))
    }

    /// 读取 syscall 输入寄存器并推进 syscall 指令 PC。
    ///
    /// # Returns
    ///
    /// `(number, 六个参数寄存器, syscall 指令 PC)`。
    pub(crate) fn take_syscall_request(&self) -> (usize, [usize; 6], usize) {
        self.thread().user_context.with(|context| {
            let request = context.take_syscall_request();
            (request.number(), request.arguments(), request.instruction())
        })
    }

    /// 写入 syscall 返回值。
    pub(crate) fn complete_syscall(&self, completion: crate::arch::context::SyscallCompletion) {
        self.thread()
            .user_context
            .with(|context| context.complete_syscall(completion));
    }

    /// 当前用户 PC。
    pub(crate) fn user_program_counter(&self) -> usize {
        self.thread()
            .user_context
            .with(|context| context.program_counter())
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
        if note != linux_abi::ptrace::NT_PRSTATUS {
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
