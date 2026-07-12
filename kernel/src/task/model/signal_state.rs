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
    Terminate(i32),
}

const UNBLOCKABLE_SIGNAL_MASK: u64 = (1u64 << (9 - 1)) | (1u64 << (19 - 1));

pub(super) fn normalize_signal_mask(mask: u64) -> u64 {
    mask & !UNBLOCKABLE_SIGNAL_MASK
}

pub(super) fn signal_is_ignored(signal: usize, action: SignalAction) -> bool {
    action.handler == 1 || signal == 17 && action.handler == 0
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
