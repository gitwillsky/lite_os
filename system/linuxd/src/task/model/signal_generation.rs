use super::*;
use alloc::sync::Arc;

impl TaskControlBlock {
    /// Graph 已提交 group exit；无条件投递内部 SIGKILL，包括 init 的 sibling。
    /// 不走外部 init exemption，也不重新取得 graph；不可屏蔽/改 disposition 的 kill 没有冲突集。
    pub(in crate::task) fn queue_group_exit_signal(&self) {
        self.thread().pending_signals.lock().queue(
            syscall_abi::signal::SIGKILL,
            PendingSignal::kernel(),
            &mut PreparedSignalRecord::default(),
        );
    }

    /// 将信号发布进当前 Thread 的唯一 pending owner；普通同号合并，实时逐个排队。
    ///
    /// # Parameters
    ///
    /// - `threads`: 同一 Process 的完整 live Thread 集合，用于原子消除 stop/continue 冲突。
    /// - `signal`: Linux signal number。
    /// - `info`: 本事件 siginfo 来源；普通信号只保留首个来源。
    /// - `prepared`: 锁外实际 record/storage/recipient ucounts 准备，成功发表后移入 pending。
    ///
    /// # Returns
    ///
    /// queued/coalesced、owned conflict retirement 与本次 disposition/mask 的 default-fatal 投影；ignored 返回 false。
    ///
    /// # Errors
    ///
    /// signal 不在 `1..=64`，或需要独立 RT record 而 preparation OOM/限额耗尽时返回 `Err(())`。
    pub(in crate::task) fn queue_signal<'a>(
        &self,
        threads: impl Iterator<Item = &'a Arc<TaskControlBlock>>,
        signal: usize,
        traced: bool,
        info: PendingSignal,
        prepared: &mut PreparedSignalRecord,
    ) -> Result<(bool, RetiredSignals, bool), ()> {
        if signal == 0 || signal > 64 {
            return Err(());
        }
        let info = info.for_ptrace(traced);
        let mut state = self.process().signal_state.lock();
        let mask = self.thread().signal_mask.lock();
        state
            .with_handlers(|handlers, pending| {
                let conflicting = signal_conflicting_mask(signal);
                let mut retired = pending.discard(conflicting);
                for thread in threads {
                    retired.append(thread.thread().pending_signals.lock().discard(conflicting));
                }
                if (!traced || signal == syscall_abi::signal::SIGKILL)
                    && handlers.ignores_generation(
                        signal,
                        *mask,
                        self.is_namespace_init(),
                        self.tgid() == 1,
                        info.is_forced(),
                    )
                {
                    return Ok((false, retired, false));
                }
                if prepared.unavailable_for(signal, info) {
                    // RT 不属于 stop/continue 冲突集，retired 必为空，不在此释放收费节点。
                    assert!(conflicting == 0);
                    return Err(());
                }
                self.thread()
                    .pending_signals
                    .lock()
                    .queue(signal, info, prepared);
                // default_fatal 是本次 publication 同锁捕获的 disposition，不在 graph 层重读。
                // 否则另一个 delivery 的 SA_RESETHAND 会把已接纳的 handler 信号变成默认致命信号。
                Ok((
                    true,
                    retired,
                    *mask & (1u64 << (signal - 1)) == 0
                        && default_fatal_action(
                            signal,
                            handlers.action(signal),
                            traced,
                            self.is_namespace_init(),
                        ),
                ))
            })
            .unwrap_or_else(|| Ok((false, RetiredSignals::default(), false)))
    }

    /// 将信号发布进当前 Process 的 shared pending owner；普通同号合并，实时逐个排队。
    ///
    /// # Parameters
    ///
    /// - `threads`: 同一 Process 的完整 live Thread 集合，用于原子消除 stop/continue 冲突。
    /// - `signal`: Linux signal number。
    /// - `info`: 本事件 siginfo 来源；普通信号只保留首个来源。
    /// - `prepared`: 锁外实际 record/storage/recipient ucounts 准备，成功发表后移入 pending。
    ///
    /// # Returns
    ///
    /// queued/coalesced、owned conflict retirement 与本次 disposition/mask 的 default-fatal 投影；ignored/default-init 返回 false。
    ///
    /// # Errors
    ///
    /// signal 不在 `1..=64`，或需要独立 RT record 而 preparation OOM/限额耗尽时返回 `Err(())`。
    pub(in crate::task) fn queue_process_signal<'a>(
        &self,
        threads: impl Iterator<Item = &'a Arc<TaskControlBlock>>,
        signal: usize,
        traced: bool,
        info: PendingSignal,
        prepared: &mut PreparedSignalRecord,
    ) -> Result<(bool, RetiredSignals, bool), ()> {
        self.queue_process_signal_mask(
            &self.thread().signal_mask,
            threads,
            signal,
            traced,
            info,
            prepared,
        )
    }

    /// PIDTYPE_TGID/PGID 从原 leader 的实际 mask 生成；leader zombie 不借某个 worker 的 mask。
    pub(in crate::task) fn queue_leader_process_signal<'a>(
        &self,
        threads: impl Iterator<Item = &'a Arc<TaskControlBlock>>,
        signal: usize,
        traced: bool,
        info: PendingSignal,
        prepared: &mut PreparedSignalRecord,
    ) -> Result<(bool, RetiredSignals, bool), ()> {
        let mask = self.process().leader_signal_mask.lock().clone();
        self.queue_process_signal_mask(&mask, threads, signal, traced, info, prepared)
    }

    fn queue_process_signal_mask<'a>(
        &self,
        recipient_mask: &crate::sync::Mutex<u64>,
        threads: impl Iterator<Item = &'a Arc<TaskControlBlock>>,
        signal: usize,
        traced: bool,
        info: PendingSignal,
        prepared: &mut PreparedSignalRecord,
    ) -> Result<(bool, RetiredSignals, bool), ()> {
        if signal == 0 || signal > 64 {
            return Err(());
        }
        let info = info.for_ptrace(traced);
        let mut state = self.process().signal_state.lock();
        let mask = recipient_mask.lock();
        state
            .with_handlers(|handlers, pending| {
                let conflicting = signal_conflicting_mask(signal);
                let mut retired = pending.discard(conflicting);
                let mut unblocked = false;
                for thread in threads {
                    let thread_mask = &*thread.thread().signal_mask;
                    let blocked = if core::ptr::eq(thread_mask, recipient_mask) {
                        *mask
                    } else {
                        *thread_mask.lock()
                    };
                    unblocked |= blocked & (1u64 << (signal - 1)) == 0;
                    retired.append(thread.thread().pending_signals.lock().discard(conflicting));
                }
                if (!traced || signal == syscall_abi::signal::SIGKILL)
                    && handlers.ignores_generation(
                        signal,
                        *mask,
                        self.is_namespace_init(),
                        self.tgid() == 1,
                        info.is_forced(),
                    )
                {
                    return Ok((false, retired, false));
                }
                if prepared.unavailable_for(signal, info) {
                    // RT 不属于 stop/continue 冲突集，retired 必为空，不在此释放收费节点。
                    assert!(conflicting == 0);
                    return Err(());
                }
                pending.queue(signal, info, prepared);
                // default_fatal 是本次 publication 同锁捕获的 disposition，不在 graph 层重读。
                // 否则另一个 delivery 的 SA_RESETHAND 会把已接纳的 handler 信号变成默认致命信号。
                Ok((
                    true,
                    retired,
                    unblocked
                        && default_fatal_action(
                            signal,
                            handlers.action(signal),
                            traced,
                            self.is_namespace_init(),
                        ),
                ))
            })
            .unwrap_or_else(|| Ok((false, RetiredSignals::default(), false)))
    }
}

/// 在 publication 事务内投影 default-fatal disposition，与同锁捕获的接收 mask 合并。
fn default_fatal_action(
    signal: usize,
    action: SignalAction,
    traced: bool,
    namespace_init: bool,
) -> bool {
    action.handler == 0
        && (!traced || signal == syscall_abi::signal::SIGKILL)
        && (!namespace_init || signal == syscall_abi::signal::SIGKILL)
        && !super::signal_is_ignored(signal, action)
        && !matches!(signal, 3..=8 | 11 | 19..=22 | 24 | 25 | 31)
}

fn signal_conflicting_mask(signal: usize) -> u64 {
    const SIGCONT_MASK: u64 = 1u64 << (18 - 1);
    const STOP_MASK: u64 =
        (1u64 << (19 - 1)) | (1u64 << (20 - 1)) | (1u64 << (21 - 1)) | (1u64 << (22 - 1));
    if signal == syscall_abi::signal::SIGCONT {
        STOP_MASK
    } else if matches!(
        signal,
        syscall_abi::signal::SIGSTOP..=syscall_abi::signal::SIGTTOU
    ) {
        SIGCONT_MASK
    } else {
        0
    }
}

impl TaskControlBlock {
    /// 原 leader 的 canonical recipient UID/UserNS 决定 RLIMIT_SIGPENDING 收费，不能换成 worker。
    pub(in crate::task) fn prepare_leader_signal_record(
        &self,
        signal: usize,
        info: PendingSignal,
    ) -> PreparedSignalRecord {
        if !(1..=64).contains(&signal) || signal == syscall_abi::signal::SIGKILL {
            return PreparedSignalRecord::default();
        }
        let credentials = super::leader_credential_snapshot(&self.process().leader_credentials);
        PreparedSignalRecord::prepare(
            signal,
            info,
            credentials.user_namespace,
            credentials.uids[0],
            self.resource_limit(super::RLIMIT_SIGPENDING)
                .expect("RLIMIT_SIGPENDING exists")
                .soft,
        )
    }
    pub(in crate::task) fn prepare_signal_record(
        &self,
        signal: usize,
        info: PendingSignal,
    ) -> PreparedSignalRecord {
        if !(1..=64).contains(&signal) || signal == syscall_abi::signal::SIGKILL {
            return PreparedSignalRecord::default();
        }
        let (namespace, uid) = self.user_resource_owner();
        PreparedSignalRecord::prepare(
            signal,
            info,
            namespace,
            uid,
            self.resource_limit(super::RLIMIT_SIGPENDING)
                .expect("RLIMIT_SIGPENDING exists")
                .soft,
        )
    }
    /// graph 已冻结 exec group；所有 SI_TIMER 节点仅摘除，不在 guard 内释放原配额。
    pub(in crate::task) fn discard_timer_signals<'a>(
        &self,
        threads: impl Iterator<Item = &'a Arc<TaskControlBlock>>,
    ) -> RetiredSignals {
        let mut state = self.process().signal_state.lock();
        let mut retired = state.pending.discard_timer_notifications();
        for thread in threads {
            retired.append(
                thread
                    .thread()
                    .pending_signals
                    .lock()
                    .discard_timer_notifications(),
            );
        }
        retired
    }
    /// graph removal 已完成后消费 thread-private pending；TCB/proc observer 不延长收费。
    pub(in crate::task) fn retire_thread_signals(&self) {
        let retired = self.thread().pending_signals.lock().discard(u64::MAX);
        drop(retired);
    }
}
