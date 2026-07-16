/// @description Linux RV64 signal disposition 的 kernel 表示。
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
    value: u64,
}

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
        }
    }

    /// @description 编码 Linux RV64 128-byte `siginfo_t` 公共头与 kill/SIGCHLD union 字段。
    ///
    /// @param signal Linux signal number。
    /// @return 完整零初始化的 ABI 字节。
    pub(crate) fn encode(self, signal: usize) -> [u8; 128] {
        let mut bytes = [0u8; 128];
        bytes[0..4].copy_from_slice(&(signal as i32).to_ne_bytes());
        bytes[8..12].copy_from_slice(&self.code.to_ne_bytes());
        bytes[16..20].copy_from_slice(&self.pid.to_ne_bytes());
        if self.code == -2 {
            bytes[20..24].copy_from_slice(&self.status.to_ne_bytes());
            bytes[24..32].copy_from_slice(&self.value.to_ne_bytes());
        } else {
            bytes[24..28].copy_from_slice(&self.status.to_ne_bytes());
        }
        bytes
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

#[repr(C)]
#[derive(Clone, Copy)]
struct UserSignalStack {
    sp: usize,
    flags: i32,
    padding: u32,
    size: usize,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct SignalMachineContext {
    regs: [usize; 32],
    fp: [u8; 528],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct SignalUserContext {
    flags: usize,
    link: usize,
    stack: UserSignalStack,
    signal_mask: u64,
    unused: [u8; 120],
    context: SignalMachineContext,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct RtSignalFrame {
    info: [u8; 128],
    context: SignalUserContext,
}

const _: () = {
    assert!(core::mem::size_of::<SignalMachineContext>() == 784);
    assert!(core::mem::size_of::<SignalUserContext>() == 952);
    assert!(core::mem::size_of::<RtSignalFrame>() == 1080);
};

impl TaskControlBlock {
    fn apply_syscall_restart(&self, context: &mut TrapContext) {
        let Some(restart) = self.thread.syscall_restart.lock().take() else {
            return;
        };
        assert_eq!(
            context.sepc,
            restart
                .ecall_pc
                .checked_add(4)
                .expect("restart ecall PC overflow"),
            "restart record does not match interrupted ecall"
        );
        context.x[10..16].copy_from_slice(&restart.args);
        context.x[17] = restart.syscall_id;
        context.sepc = restart.ecall_pc;
    }

    /// @description 在 trap return 前选择 pending signal，并构造唯一 RV64 rt frame。
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
            // Linux 的 SIGNAL_UNKILLABLE 语义只压制 PID 1 的默认 disposition；显式
            // handler 仍需执行，强制同步 fault 则不经过 pending delivery 入口。
            if self.tgid() == crate::task::pid::INIT_PID && action.handler == 0 {
                continue;
            }
            if signal_is_default_stop(signal, action) {
                if signal != 19
                    && super::super::task_manager::current_process_group_is_orphaned(self.tgid())
                {
                    continue;
                }
                self.thread.suspend_restore_mask.lock().take();
                let mut context = self.load_trap_context();
                self.apply_syscall_restart(&mut context);
                self.set_trap_context(context);
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

            let mut context = self.load_trap_context();
            if action.flags & SA_RESTART != 0 {
                self.apply_syscall_restart(&mut context);
            } else {
                self.thread.syscall_restart.lock().take();
            }
            let frame_size = core::mem::size_of::<RtSignalFrame>();
            let (frame_address, saved_stack) =
                self.signal_frame_stack(context.x[2], action.flags & SA_ONSTACK != 0, frame_size)?;
            let mut registers = [0usize; 32];
            registers[0] = context.sepc;
            registers[1..].copy_from_slice(&context.x[1..]);
            let mut fp = [0u8; 528];
            for (index, value) in context.f.iter().enumerate() {
                fp[index * 8..index * 8 + 8].copy_from_slice(&value.to_ne_bytes());
            }
            fp[256..260].copy_from_slice(&(context.fcsr as u32).to_ne_bytes());
            let frame = RtSignalFrame {
                info: signal_info.encode(signal),
                context: SignalUserContext {
                    flags: 0,
                    link: 0,
                    stack: UserSignalStack {
                        sp: saved_stack.sp,
                        flags: saved_stack.flags,
                        padding: 0,
                        size: saved_stack.size,
                    },
                    signal_mask: old_mask,
                    unused: [0; 120],
                    context: SignalMachineContext {
                        regs: registers,
                        fp,
                    },
                },
            };
            // SAFETY: repr(C) frame contains no references or padding with uninitialized data.
            let bytes = unsafe {
                core::slice::from_raw_parts(
                    (&frame as *const RtSignalFrame).cast::<u8>(),
                    frame_size,
                )
            };
            self.copy_to_user(frame_address, bytes)?;
            self.commit_signal_stack_delivery();
            let mut new_mask = old_mask | action.mask;
            if action.flags & SA_NODEFER == 0 {
                new_mask |= 1u64 << (signal - 1);
            }
            *self.thread.signal_mask.lock() = normalize_signal_mask(new_mask);
            if action.flags & SA_RESETHAND != 0 {
                self.process.signal_state.lock().actions[signal] = SignalAction::default();
            }
            context.x[1] = crate::memory::signal_trampoline_entry();
            context.x[2] = frame_address;
            context.x[10] = signal;
            context.x[11] = frame_address;
            context.x[12] = frame_address + 128;
            context.sepc = action.handler;
            self.set_trap_context(context);
            return Ok(SignalDelivery::None);
        }
    }

    /// @description 从当前用户 sp 读取并恢复唯一 RV64 rt signal frame。
    ///
    /// @return 恢复后的用户 `a0`。
    /// @errors frame 不可读或包含未支持 extension 时返回 `UserAccessError`。
    pub(crate) fn restore_signal_frame(&self) -> Result<usize, UserAccessError> {
        let frame_address = self.load_trap_context().x[2];
        let mut bytes = [0u8; core::mem::size_of::<RtSignalFrame>()];
        self.copy_from_user(frame_address, &mut bytes)?;
        // SAFETY: byte array has the exact size/alignment-independent representation; read_unaligned
        // produces an owned frame before any field is inspected.
        let frame = unsafe { core::ptr::read_unaligned(bytes.as_ptr().cast::<RtSignalFrame>()) };
        // Linux 将 extra-extension 的 reserved word 与 END header 覆盖在 FP union 尾部。
        // 这里尚不支持 vector/CFI extension；若不校验这 12 bytes，损坏或伪造的扩展链会被
        // 静默接受，并把一个并非本实现能够完整恢复的上下文当作有效 frame。
        if frame.context.context.fp[516..528]
            .iter()
            .any(|byte| *byte != 0)
        {
            return Err(UserAccessError::Fault);
        }
        let mut context = self.load_trap_context();
        context.sepc = frame.context.context.regs[0];
        context.x[1..].copy_from_slice(&frame.context.context.regs[1..]);
        for index in 0..32 {
            context.f[index] = u64::from_ne_bytes(
                frame.context.context.fp[index * 8..index * 8 + 8]
                    .try_into()
                    .unwrap(),
            );
        }
        context.fcsr =
            u32::from_ne_bytes(frame.context.context.fp[256..260].try_into().unwrap()) as usize;
        *self.thread.signal_mask.lock() = normalize_signal_mask(frame.context.signal_mask);
        self.restore_signal_stack(
            context.x[2],
            SignalStack {
                sp: frame.context.stack.sp,
                flags: frame.context.stack.flags,
                size: frame.context.stack.size,
            },
        );
        let result = context.x[10];
        self.set_trap_context(context);
        Ok(result)
    }
}
