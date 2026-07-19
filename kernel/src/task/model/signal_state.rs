/// @description Linux 64-bit signal disposition 的 kernel 表示。
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct SignalAction {
    pub(crate) handler: usize,
    pub(crate) flags: usize,
    pub(crate) mask: u64,
}

/// @description trap return 完成 pending signal 选择后的唯一控制结果。
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
    action.handler == 1 || action.handler == 0 && matches!(signal, 17 | 18 | 23 | 28)
}

pub(super) fn signal_is_default_stop(signal: usize, action: SignalAction) -> bool {
    action.handler == 0 && matches!(signal, 19..=22)
}

/// @description coalesced standard signal 随 pending bit 保存的最小 Linux siginfo 来源。
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct PendingSignal {
    code: i32,
    pid: i32,
    status: i32,
    fault_layout: bool,
    forced: bool,
    value: u64,
}

const _: () = assert!(core::mem::size_of::<PendingSignal>() == 24);

impl PendingSignal {
    /// @description 构造 thread-directed signal 的 `SI_TKILL` 来源。
    ///
    /// @param pid 发送者 thread group ID。
    /// @return 可用于 signal frame 或 `rt_sigtimedwait` 的来源。
    pub(crate) fn thread_directed(pid: usize) -> Self {
        Self {
            code: -6,
            pid: pid as i32,
            status: 0,
            value: 0,
            ..Self::default()
        }
    }

    /// @description 构造 process-directed signal 的 `SI_USER` 来源。
    ///
    /// @param pid 发送者 thread group ID。
    /// @return 可用于 signal frame 或 `rt_sigtimedwait` 的来源。
    pub(crate) fn process_directed(pid: usize) -> Self {
        Self {
            code: 0,
            pid: pid as i32,
            status: 0,
            value: 0,
            ..Self::default()
        }
    }

    /// @description 构造正常退出 child 的 `CLD_EXITED` 来源。
    ///
    /// @param pid 退出 child 的 thread group ID。
    /// @param status child exit status。
    /// @return SIGCHLD 的来源。
    pub(crate) fn child_exited(pid: usize, status: i32) -> Self {
        Self {
            code: 1,
            pid: pid as i32,
            status,
            value: 0,
            ..Self::default()
        }
    }

    /// @description 构造由 signal 终止 child 的 `CLD_KILLED` 来源。
    ///
    /// @param pid 退出 child 的 thread group ID。
    /// @param signal 终止 child 的 signal number。
    /// @return SIGCHLD 的来源。
    pub(crate) fn child_killed(pid: usize, signal: usize) -> Self {
        Self {
            code: 2,
            pid: pid as i32,
            status: signal as i32,
            value: 0,
            ..Self::default()
        }
    }

    /// @description 构造 job-control stop 完成时的 `CLD_STOPPED` 来源。
    ///
    /// @param pid 停止的 child thread group ID。
    /// @param signal 触发 group stop 的 signal number。
    /// @return parent SIGCHLD 与 wait status 共用的来源。
    pub(crate) fn child_stopped(pid: usize, signal: usize) -> Self {
        Self {
            code: 5,
            pid: pid as i32,
            status: signal as i32,
            value: 0,
            ..Self::default()
        }
    }

    /// @description 构造 stopped child 恢复时的 `CLD_CONTINUED` 来源。
    ///
    /// @param pid 恢复的 child thread group ID。
    /// @return `si_status=SIGCONT` 的 parent SIGCHLD 来源。
    pub(crate) fn child_continued(pid: usize) -> Self {
        Self {
            code: 6,
            pid: pid as i32,
            status: 18,
            value: 0,
            ..Self::default()
        }
    }

    /// @description 构造由 kernel TTY line discipline 产生的 `SI_KERNEL` signal 来源。
    ///
    /// @return pid/uid/status 为零的 kernel 来源。
    pub(crate) fn kernel() -> Self {
        Self {
            code: 128,
            pid: 0,
            status: 0,
            value: 0,
            ..Self::default()
        }
    }

    /// @description 构造由当前 instruction/data fault 强制投递的同步 signal 来源。
    /// @param code signal-specific positive `si_code`，例如 `ILL_ILLOPC`。
    /// @param address 触发 fault 的用户虚拟地址。
    /// @return fault union 在 offset 16 编码 `si_addr`，并标记为绕过 PID 1 默认豁免。
    pub(crate) fn synchronous_fault(code: i32, address: usize) -> Self {
        assert!(code > 0, "synchronous fault si_code must be positive");
        Self {
            code,
            fault_layout: true,
            forced: true,
            value: address as u64,
            ..Self::default()
        }
    }

    /// 构造 POSIX timer expiration 的 `SI_TIMER` 来源。
    ///
    /// @param id 创建进程内的 timer ID。
    /// @param overrun 最近一次 expiration 的 overrun count。
    /// @param value `sigev_value` 的原始 64-bit union payload。
    /// @return 可供 signal frame 与 `rt_sigtimedwait` 观察的 timer siginfo。
    pub(crate) fn timer(id: i32, overrun: i32, value: u64) -> Self {
        Self {
            code: -2,
            pid: id,
            status: overrun,
            value,
            ..Self::default()
        }
    }

    /// @description 编码 Linux 64-bit 128-byte `siginfo_t` 公共头与 kill/SIGCHLD/fault union 字段。
    ///
    /// @param signal Linux signal number。
    /// @return 完整零初始化的 ABI 字节。
    pub(crate) fn encode(self, signal: usize) -> [u8; 128] {
        let mut bytes = [0u8; 128];
        bytes[0..4].copy_from_slice(&(signal as i32).to_ne_bytes());
        bytes[8..12].copy_from_slice(&self.code.to_ne_bytes());
        if self.fault_layout {
            bytes[16..24].copy_from_slice(&self.value.to_ne_bytes());
        } else {
            bytes[16..20].copy_from_slice(&self.pid.to_ne_bytes());
        }
        if self.code == -2 {
            bytes[20..24].copy_from_slice(&self.status.to_ne_bytes());
            bytes[24..32].copy_from_slice(&self.value.to_ne_bytes());
        } else if !self.fault_layout {
            bytes[24..28].copy_from_slice(&self.status.to_ne_bytes());
        }
        bytes
    }

    fn is_forced_fault(self) -> bool {
        self.fault_layout && self.forced
    }
}

#[derive(Debug)]
pub(super) struct PendingSignals {
    pub(super) bits: u64,
    info: [PendingSignal; 65],
}

impl PendingSignals {
    pub(super) fn new() -> Self {
        Self {
            bits: 0,
            info: [PendingSignal::default(); 65],
        }
    }

    pub(super) fn queue(&mut self, signal: usize, info: PendingSignal) {
        let bit = 1u64 << (signal - 1);
        if self.bits & bit == 0 {
            self.info[signal] = info;
            self.bits |= bit;
        } else {
            super::synchronous_fault::merge_forced(&mut self.info[signal].forced, info.forced);
        }
    }

    pub(super) fn take(&mut self, mask: u64) -> Option<(usize, PendingSignal)> {
        let available = self.bits & mask;
        if available == 0 {
            return None;
        }
        let signal = available.trailing_zeros() as usize + 1;
        self.bits &= !(1u64 << (signal - 1));
        Some((signal, self.info[signal]))
    }

    pub(super) fn discard(&mut self, mask: u64) {
        self.bits &= !mask;
    }
}

/// @description Process 共享 disposition 与 process-directed pending 的唯一同锁 owner。
#[derive(Debug)]
pub(super) struct ProcessSignalState {
    pub(super) actions: [SignalAction; 65],
    pub(super) pending: PendingSignals,
}

impl ProcessSignalState {
    /// @description 创建 fork/new Process 使用的 disposition 与空 shared pending owner。
    ///
    /// @param actions fork 继承或新 Process 默认的 disposition table。
    /// @return shared pending 为空的新状态。
    pub(super) fn new(actions: [SignalAction; 65]) -> Self {
        Self {
            actions,
            pending: PendingSignals::new(),
        }
    }

    /// @description 按 execve 规则重置 caught disposition，保留 SIG_IGN 与 shared pending。
    ///
    /// @return 无返回值；丢失 pending 会让 exec 后应交付的 signal 静默消失。
    pub(super) fn reset_dispositions_for_exec(&mut self) {
        for action in &mut self.actions {
            if action.handler != 1 {
                *action = SignalAction::default();
            }
        }
    }
}

impl TaskControlBlock {
    /// @description 向当前 Thread 强制投递一个 synchronous fault signal。
    /// @param signal `1..=64` 的 Linux signal number。
    /// @param info 必须由 `PendingSignal::synchronous_fault` 构造。
    /// @return disposition、mask 与 thread-pending 已在唯一锁事务中发布时成功。
    /// @errors signal 非法或 info 不是同步 fault 来源时返回 `Err(())`。
    pub(crate) fn queue_synchronous_fault(
        &self,
        signal: usize,
        info: PendingSignal,
    ) -> Result<(), ()> {
        if signal == 0 || signal > 64 || !info.is_forced_fault() {
            return Err(());
        }
        let mut signal_mask = self.thread.signal_mask.lock();
        let mut state = self.process.signal_state.lock();
        let policy = super::synchronous_fault::force_synchronous_fault(
            signal,
            state.actions[signal].handler,
            *signal_mask,
        );
        if policy.reset_to_default {
            state.actions[signal] = SignalAction::default();
        }
        *signal_mask = policy.signal_mask;
        self.thread.pending_signals.lock().queue(signal, info);
        Ok(())
    }

    /// @description 将 standard signal 及首个来源合并进当前 Thread 的 pending state。
    ///
    /// @param threads 同一 Process 的完整 live Thread 集合，用于原子消除 stop/continue 冲突。
    /// @param signal Linux signal number。
    /// @param info 首次发布时保存的 siginfo 来源。
    /// @return signal 成功合并或按 disposition 丢弃时返回 `Ok(())`。
    /// @errors signal 不在 `1..=64` 时返回 `Err(())`。
    pub(in crate::task) fn queue_signal<'a>(
        &self,
        threads: impl Iterator<Item = &'a Arc<TaskControlBlock>>,
        signal: usize,
        info: PendingSignal,
    ) -> Result<(), ()> {
        if signal == 0 || signal > 64 {
            return Err(());
        }
        let mut state = self.process.signal_state.lock();
        let conflicting = signal_conflicting_mask(signal);
        if conflicting != 0 {
            state.pending.discard(conflicting);
            for thread in threads {
                thread.thread.pending_signals.lock().discard(conflicting);
            }
        }
        let action = state.actions[signal];
        if action.handler == 1 {
            return Ok(());
        }
        self.thread.pending_signals.lock().queue(signal, info);
        Ok(())
    }

    /// @description 将 standard signal 合并进当前 Process 的 shared pending state。
    ///
    /// @param threads 同一 Process 的完整 live Thread 集合，用于原子消除 stop/continue 冲突。
    /// @param signal Linux signal number。
    /// @param info 首次发布时保存的 siginfo 来源。
    /// @return queued/已 coalesce 返回 true，显式 SIG_IGN 丢弃返回 false。
    /// @errors signal 不在 `1..=64` 时返回 `Err(())`。
    pub(in crate::task) fn queue_process_signal<'a>(
        &self,
        threads: impl Iterator<Item = &'a Arc<TaskControlBlock>>,
        signal: usize,
        info: PendingSignal,
    ) -> Result<bool, ()> {
        if signal == 0 || signal > 64 {
            return Err(());
        }
        let mut state = self.process.signal_state.lock();
        let conflicting = signal_conflicting_mask(signal);
        state.pending.discard(conflicting);
        for thread in threads {
            thread.thread.pending_signals.lock().discard(conflicting);
        }
        if state.actions[signal].handler == 1 {
            return Ok(false);
        }
        state.pending.queue(signal, info);
        Ok(true)
    }
}

fn signal_conflicting_mask(signal: usize) -> u64 {
    const SIGCONT_MASK: u64 = 1u64 << (18 - 1);
    const STOP_MASK: u64 =
        (1u64 << (19 - 1)) | (1u64 << (20 - 1)) | (1u64 << (21 - 1)) | (1u64 << (22 - 1));
    if signal == 18 {
        STOP_MASK
    } else if matches!(signal, 19..=22) {
        SIGCONT_MASK
    } else {
        0
    }
}
use alloc::sync::Arc;

use super::*;
use crate::arch::context::{SIGNAL_FRAME_SIZE, SignalFrame, SignalStack as ArchSignalStack};

impl TaskControlBlock {
    fn apply_syscall_restart(&self, context: &mut UserContext) {
        let Some(restart) = self.thread.syscall_restart.lock().take() else {
            return;
        };
        context.restart_syscall(restart.syscall_id, restart.args, restart.ecall_pc);
    }

    /// @description 在 trap return 前选择 pending signal，并委托编译期 arch codec 构造 frame。
    ///
    /// @return 无可交付 signal/handler frame 已就绪时返回 `None`；默认终止返回状态码。
    /// @errors 用户栈 frame 无法完整写入时返回 `UserAccessError`。
    pub(crate) fn prepare_signal_delivery(&self) -> Result<SignalDelivery, UserAccessError> {
        const SA_RESTART: usize = 0x1000_0000;
        const SA_ONSTACK: usize = 0x0800_0000;
        const SA_NODEFER: usize = 0x4000_0000;
        const SA_RESETHAND: usize = 0x8000_0000;
        loop {
            let selection_mask = *self.thread.signal_mask.lock();
            let selected = {
                let mut state = self.process.signal_state.lock();
                let mut pending = self.thread.pending_signals.lock();
                pending
                    .take(!selection_mask)
                    .or_else(|| state.pending.take(!selection_mask))
                    .map(|(signal, info)| (signal, info, state.actions[signal]))
            };
            let Some((signal, signal_info, action)) = selected else {
                self.thread.syscall_restart.lock().take();
                return Ok(SignalDelivery::None);
            };
            if signal_is_ignored(signal, action) {
                continue;
            }
            // Linux 的 SIGNAL_UNKILLABLE 语义只压制 PID 1 的异步默认 disposition；显式
            // handler 仍需执行，force_sig_info 发布的同步 fault 必须绕过该豁免。
            if self.tgid() == crate::task::pid::INIT_PID
                && action.handler == 0
                && !signal_info.forced
            {
                continue;
            }
            if signal_is_default_stop(signal, action) {
                if signal != 19
                    && super::super::task_manager::current_process_group_is_orphaned(self.tgid())
                {
                    continue;
                }
                self.thread.suspend_restore_mask.lock().take();
                self.thread
                    .user_context
                    .with(|context| self.apply_syscall_restart(context));
                return Ok(SignalDelivery::Stop(signal));
            }
            if action.handler == 0 {
                self.thread.suspend_restore_mask.lock().take();
                self.thread.syscall_restart.lock().take();
                return Ok(SignalDelivery::Terminate(signal));
            }

            let old_mask = self
                .thread
                .suspend_restore_mask
                .lock()
                .take()
                .unwrap_or(selection_mask);

            let user_stack_pointer = self.thread.user_context.with(|context| {
                if action.flags & SA_RESTART != 0 {
                    self.apply_syscall_restart(context);
                } else {
                    self.thread.syscall_restart.lock().take();
                }
                context.stack_pointer()
            });
            let (frame_address, saved_stack) = self.signal_frame_stack(
                user_stack_pointer,
                action.flags & SA_ONSTACK != 0,
                SIGNAL_FRAME_SIZE,
            )?;
            let frame = self.thread.user_context.with(|context| {
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
            *self.thread.signal_mask.lock() = normalize_signal_mask(new_mask);
            if action.flags & SA_RESETHAND != 0 {
                self.process.signal_state.lock().actions[signal] = SignalAction::default();
            }
            self.thread.user_context.with(|context| {
                context.enter_signal_handler(
                    crate::memory::signal_trampoline_entry(),
                    frame_address,
                    signal,
                    action.handler,
                );
            });
            return Ok(SignalDelivery::None);
        }
    }

    /// @description 从当前用户 sp 读取并由编译期 arch codec 恢复 rt signal frame。
    ///
    /// @return 恢复后的用户 `a0`。
    /// @errors frame 不可读或包含未支持 extension 时返回 `UserAccessError`。
    pub(crate) fn restore_signal_frame(&self) -> Result<usize, UserAccessError> {
        let frame_address = self.user_stack_pointer();
        let mut frame = SignalFrame::zeroed();
        self.copy_from_user(frame_address, frame.as_bytes_mut())?;
        let (result, signal_mask, signal_stack, restored_sp) =
            self.thread.user_context.with(|context| {
                let (result, signal_mask, signal_stack) = context
                    .restore_signal_frame(&frame)
                    .map_err(|_| UserAccessError::Fault)?;
                Ok::<_, UserAccessError>((
                    result,
                    signal_mask,
                    signal_stack,
                    context.stack_pointer(),
                ))
            })?;
        *self.thread.signal_mask.lock() = normalize_signal_mask(signal_mask);
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
}
