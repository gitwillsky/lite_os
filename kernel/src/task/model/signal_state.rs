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
        bytes[24..28].copy_from_slice(&self.status.to_ne_bytes());
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
        state.pending.discard(conflicting);
        for thread in threads {
            thread.thread.pending_signals.lock().discard(conflicting);
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

use super::TaskControlBlock;
