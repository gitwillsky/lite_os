/// sigreturn 的唯一失败结果，区分资源耗尽与架构定义的坏帧来源。
#[derive(Debug, Clone, Copy)]
pub(crate) enum SignalFrameRestoreError {
    /// user-copy/缺页的资源耗尽，不能作为可捕获地址 fault 返回。
    OutOfMemory,
    /// None 为 SI_KERNEL；Some 为对应失败阶段的架构 fault address。
    Invalid { address: Option<usize> },
}

/// trap return 完成 pending signal 选择后的唯一控制结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SignalDelivery {
    None,
    Stop(usize),
    Terminate(usize),
}

const UNBLOCKABLE_SIGNAL_MASK: u64 = (1u64 << (9 - 1)) | (1u64 << (19 - 1));

pub(super) fn normalize_signal_mask(mask: u64) -> u64 {
    mask & !UNBLOCKABLE_SIGNAL_MASK
}

pub(super) fn signal_is_ignored(signal: usize, action: SignalAction) -> bool {
    action.handler == 1
        || action.handler == 0
            && matches!(
                signal,
                syscall_abi::signal::SIGCHLD
                    | syscall_abi::signal::SIGCONT
                    | syscall_abi::signal::SIGURG
                    | syscall_abi::signal::SIGWINCH
            )
}

pub(super) fn signal_is_default_stop(signal: usize, action: SignalAction) -> bool {
    action.handler == 0
        && matches!(
            signal,
            syscall_abi::signal::SIGSTOP..=syscall_abi::signal::SIGTTOU
        )
}

impl TaskControlBlock {
    /// 向当前 Thread 强制投递一个 synchronous fault signal。
    ///
    /// # Parameters
    ///
    /// - `signal`: `1..=64` 的 Linux signal number。
    /// - `info`: 必须由 `PendingSignal::synchronous_fault` 或 `seccomp` 构造。
    ///
    /// # Returns
    ///
    /// disposition、mask 与 thread-pending 已在唯一锁事务中发布时成功。
    ///
    /// # Errors
    ///
    /// signal 非法或 info 不是同步 fault 来源时返回 `Err(())`。
    pub(crate) fn queue_synchronous_fault(
        &self,
        signal: usize,
        info: PendingSignal,
    ) -> Result<(), ()> {
        if signal == 0 || signal > 64 || !info.is_forced_fault() {
            return Err(());
        }
        let mut prepared = self.prepare_signal_record(signal, info);
        let mut signal_mask = self.thread().signal_mask.lock();
        self.process()
            .signal_state
            .lock()
            .with_handlers(|handlers, _| {
                let policy = super::synchronous_fault::force_synchronous_fault(
                    signal,
                    handlers.action(signal).handler,
                    *signal_mask,
                );
                if policy.reset_to_default {
                    let mut action = handlers.action(signal);
                    action.handler = 0;
                    handlers.replace(signal, action);
                }
                *signal_mask = policy.signal_mask;
                self.thread()
                    .pending_signals
                    .lock()
                    .queue(signal, info, &mut prepared);
            });
        Ok(())
    }
}

use super::*;
use crate::arch::context::{SIGNAL_FRAME_SIZE, SignalFrame, SignalStack as ArchSignalStack};

impl TaskControlBlock {
    fn apply_syscall_restart(&self, context: &mut UserContext, handler_flags: Option<usize>) {
        match self
            .thread()
            .syscall_restart
            .lock()
            .resolve_signal(handler_flags)
        {
            Some(super::syscall_restart::RestartResolution::Replay(restart)) => {
                context.restart_syscall(restart.syscall_id, restart.args, restart.syscall_pc)
            }
            Some(super::syscall_restart::RestartResolution::Traced {
                instruction,
                replay_number,
            }) => context.finish_ptrace_restart(instruction, replay_number),
            None => {}
        }
    }

    /// 在 trap return 前选择 pending signal，并委托编译期 arch codec 构造 frame。
    ///
    /// # Returns
    ///
    /// 无可交付 signal/handler frame 已就绪时返回 `None`；默认终止返回状态码。
    ///
    /// # Errors
    ///
    /// 用户栈 frame 无法完整写入时返回 `UserAccessError`。
    pub(crate) fn prepare_signal_delivery(&self) -> Result<SignalDelivery, UserAccessError> {
        const SA_ONSTACK: usize = 0x0800_0000;
        const SA_NODEFER: usize = 0x4000_0000;
        loop {
            let selection_mask = *self.thread().signal_mask.lock();
            let selected = self.take_pending_signal(!selection_mask);
            let Some((signal, signal_info, queue)) = selected else {
                self.thread()
                    .user_context
                    .with(|context| self.apply_syscall_restart(context, None));
                return Ok(SignalDelivery::None);
            };
            let Some((signal, signal_info)) = super::super::process_table::ptrace::intercept_signal(
                self,
                signal,
                signal_info,
                queue,
            ) else {
                continue;
            };
            let Some(action) = self
                .process()
                .signal_state
                .lock()
                .with_handlers(|handlers, _| handlers.select(signal))
            else {
                continue;
            };
            if signal_is_ignored(signal, action) {
                continue;
            }
            // Linux 的 SIGNAL_UNKILLABLE 语义只压制 PID 1 的异步默认 disposition；显式
            // handler 仍需执行，force_sig_info 发布的同步 fault 必须绕过该豁免。
            if self.is_namespace_init() && action.handler == 0 && !signal_info.bypasses_init(signal)
            {
                continue;
            }
            if signal_is_default_stop(signal, action) {
                if signal != 19
                    && super::super::process_table::current_process_group_is_orphaned(self.tgid())
                {
                    continue;
                }
                // Stop 尚未完成信号选择；保留 replay/block 与 sigsuspend mask。
                // 继续运行后若有捕获 handler，仍需按它的 disposition 决定 EINTR。
                return Ok(SignalDelivery::Stop(signal));
            }
            if action.handler == 0 {
                self.thread().suspend_restore_mask.lock().take();
                self.clear_syscall_restart();
                return Ok(SignalDelivery::Terminate(signal));
            }

            let old_mask = self
                .thread()
                .suspend_restore_mask
                .lock()
                .take()
                .unwrap_or(selection_mask);

            let user_stack_pointer = self.thread().user_context.with(|context| {
                self.apply_syscall_restart(context, Some(action.flags));
                context.stack_pointer()
            });
            let (frame_address, saved_stack) = self.signal_frame_stack(
                user_stack_pointer,
                action.flags & SA_ONSTACK != 0,
                SIGNAL_FRAME_SIZE,
            )?;
            let frame = self.thread().user_context.with(|context| {
                context.capture_signal_frame(
                    signal_info.encode(signal),
                    ArchSignalStack::new(saved_stack.sp, saved_stack.flags, saved_stack.size),
                    old_mask,
                )
            });
            self.copy_to_user(frame_address, frame.as_bytes())?;
            self.commit_signal_stack_delivery();
            let mut new_mask = old_mask | action.mask;
            if action.flags & SA_NODEFER == 0 {
                new_mask |= 1u64 << (signal - 1);
            }
            *self.thread().signal_mask.lock() = normalize_signal_mask(new_mask);
            self.thread().user_context.with(|context| {
                context.enter_signal_handler(
                    crate::memory::signal_trampoline_entry(),
                    frame_address,
                    signal,
                    action.handler,
                    action.flags,
                    action.restorer,
                );
            });
            return Ok(SignalDelivery::None);
        }
    }

    /// 从当前用户 sp 读取并由编译期 arch codec 恢复 rt signal frame。
    ///
    /// # Returns
    ///
    /// 恢复后的用户 syscall return register。
    ///
    /// # Errors
    ///
    /// 坏 frame 携带失败阶段的架构 fault address；copyin OOM 保持独立分类。
    pub(crate) fn restore_signal_frame(&self) -> Result<usize, SignalFrameRestoreError> {
        let frame_address = self.user_stack_pointer();
        if !SignalFrame::valid_user_address(frame_address) {
            return Err(SignalFrameRestoreError::Invalid {
                address: SignalFrame::fault_address(frame_address),
            });
        }
        let mut frame = SignalFrame::zeroed();
        self.copy_from_user(frame_address, frame.as_bytes_mut())
            .map_err(|error| match error {
                UserAccessError::OutOfMemory => SignalFrameRestoreError::OutOfMemory,
                _ => SignalFrameRestoreError::Invalid {
                    address: SignalFrame::fault_address(frame_address),
                },
            })?;
        // Linux 在 machine/extension validation 前恢复 mask；坏 frame 中 blocked SIGSEGV
        // 必须参与 force_sig 的 disposition 决策，不能只在成功路径发布。
        *self.thread().signal_mask.lock() = normalize_signal_mask(frame.signal_mask());
        let (result, _signal_mask, signal_stack, restored_sp) =
            self.thread().user_context.with(|context| {
                let (result, signal_mask, signal_stack) = context
                    .restore_signal_frame(&frame)
                    .map_err(|_| SignalFrameRestoreError::Invalid {
                        address: frame.restore_fault_address(),
                    })?;
                Ok::<_, SignalFrameRestoreError>((
                    result,
                    signal_mask,
                    signal_stack,
                    context.stack_pointer(),
                ))
            })?;
        self.restore_signal_stack(
            restored_sp,
            SignalStack {
                sp: signal_stack.sp(),
                flags: signal_stack.flags(),
                size: signal_stack.size(),
            },
        );
        Ok(result)
    }

    /// 由静态 frame ABI 与唯一 VMA owner 构造坏帧的 forced SIGSEGV 来源。
    ///
    /// VMA waiter metadata OOM 返回 UserAccessError；caller 必须按内存耗尽终止。
    pub(crate) fn signal_frame_fault(
        &self,
        address: Option<usize>,
    ) -> Result<PendingSignal, UserAccessError> {
        use syscall_abi::signal::{SEGV_ACCERR, SEGV_MAPERR, SI_KERNEL};
        let Some(address) = address else {
            return Ok(PendingSignal::synchronous_fault(SI_KERNEL, 0));
        };
        let space = self.address_space();
        let has_vma = space
            .memory_set
            .lock()
            .map_err(|_| UserAccessError::OutOfMemory)?
            .has_user_vma_at_or_after(address);
        Ok(PendingSignal::synchronous_fault(
            if has_vma { SEGV_ACCERR } else { SEGV_MAPERR },
            address,
        ))
    }
}
