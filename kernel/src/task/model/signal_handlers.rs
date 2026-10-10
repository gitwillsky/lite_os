//! 可跨 Process 共享的 disposition owner；pending 仍属于各自的 Thread/Process。
use alloc::sync::Arc;
use spin::Mutex;

use super::{
    PendingSignals, PreparedSignalRecord, RetiredSignals, TaskControlBlock, normalize_signal_mask,
    signal_is_ignored,
};
use crate::memory::MemoryError;

/// Linux 64-bit signal disposition 的 kernel 表示。
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct SignalAction {
    pub(crate) handler: usize,
    pub(crate) flags: usize,
    pub(crate) restorer: usize,
    pub(crate) mask: u64,
}

/// SIGCHLD disposition 的一次同锁 projection；无第二份 lifecycle state。
#[derive(Clone, Copy)]
struct ChildSignalPolicy {
    autoreap: bool,
    notify_exit: bool,
    notify_job: bool,
}

/// child notification 的 lifecycle 来源；job 始终 SIGCHLD，exit 使用真实关系上的 signal。
#[derive(Clone, Copy)]
pub(in crate::task) enum ChildSignalKind {
    Exit,
    Job,
}
/// 同锁完成的回收决定与 pending publication 结果；锁外只处理调度唤醒。
pub(in crate::task) struct ChildSignalPublication {
    pub(in crate::task) autoreap: bool,
    pub(in crate::task) queued: bool,
}

/// 唯一 disposition table；不拥有 mask、pending 或 Task 引用。
#[derive(Debug)]
pub(super) struct SignalHandlers {
    actions: [SignalAction; 65],
    wait_source: Arc<crate::file::SignalWaitSource>,
}

impl SignalHandlers {
    pub(super) fn action(&self, signal: usize) -> SignalAction {
        self.actions[signal]
    }

    fn child_policy(&self) -> ChildSignalPolicy {
        let action = self.action(syscall_abi::signal::SIGCHLD);
        ChildSignalPolicy {
            autoreap: action.handler == 1 || action.flags & 2 != 0,
            notify_exit: action.handler != 1,
            notify_job: action.handler != 1 && action.flags & 1 == 0,
        }
    }

    /// blocked ignored signal 必须留给 sigwait；init exemption 属于同锁 generation，
    /// 不能先复制 action 再另行发布 pending。
    pub(super) fn ignores_generation(
        &self,
        signal: usize,
        blocked: u64,
        namespace_init: bool,
        global_init: bool,
        force: bool,
    ) -> bool {
        if blocked & (1u64 << (signal - 1)) != 0 {
            return false;
        }
        let action = self.action(signal);
        (global_init && matches!(signal, 9 | 19))
            || (namespace_init && action.handler == 0 && !(force && matches!(signal, 9 | 19)))
            || signal_is_ignored(signal, action)
    }

    pub(super) fn replace(&mut self, signal: usize, action: SignalAction) {
        self.actions[signal] = action;
    }

    /// Linux 在选择 handler 的同锁事务内只清 handler，不能在 copy-user 后覆盖新 action。
    pub(super) fn select(&mut self, signal: usize) -> SignalAction {
        const SA_RESETHAND: usize = 0x8000_0000;
        let action = self.actions[signal];
        if action.handler > 1 && action.flags & SA_RESETHAND != 0 {
            self.actions[signal].handler = 0;
        }
        action
    }

    pub(super) fn caught_set(&self, candidates: u64) -> u64 {
        let mut result = 0;
        for (signal, &action) in self.actions.iter().enumerate().skip(1) {
            let bit = 1u64 << (signal - 1);
            if candidates & bit != 0 && !signal_is_ignored(signal, action) {
                result |= bit;
            }
        }
        result
    }

    fn reset_for_exec(&mut self) {
        for action in &mut self.actions {
            let handler = if action.handler == 1 { 1 } else { 0 };
            *action = SignalAction {
                handler,
                ..SignalAction::default()
            };
        }
    }
}

/// Group pending 与其可退休 handle 的 owner；共享表不共享 group pending。
#[derive(Debug)]
pub(super) struct ProcessSignalState {
    handlers: Option<Arc<Mutex<SignalHandlers>>>,
    pub(super) pending: PendingSignals,
}

impl ProcessSignalState {
    pub(super) fn new(handlers: Arc<Mutex<SignalHandlers>>) -> Self {
        Self {
            handlers: Some(handlers),
            pending: PendingSignals::new(),
        }
    }

    pub(super) fn try_default_handlers() -> Result<Arc<Mutex<SignalHandlers>>, MemoryError> {
        Self::try_handlers([SignalAction::default(); 65])
    }

    fn try_handlers(
        actions: [SignalAction; 65],
    ) -> Result<Arc<Mutex<SignalHandlers>>, MemoryError> {
        let wait_source =
            crate::file::SignalWaitSource::try_new().map_err(|_| MemoryError::OutOfMemory)?;
        Arc::try_new(Mutex::new(SignalHandlers {
            actions,
            wait_source,
        }))
        .map_err(|_| MemoryError::OutOfMemory)
    }

    /// 调用者已持 group state；closure 不得进入 graph、MM、allocator 或另一 group state。
    pub(super) fn with_handlers<T>(
        &mut self,
        operation: impl FnOnce(&mut SignalHandlers, &mut PendingSignals) -> T,
    ) -> Option<T> {
        let mut handlers = self.handlers.as_ref()?.lock();
        Some(operation(&mut handlers, &mut self.pending))
    }
}

impl crate::file::FileReadinessContext for TaskControlBlock {
    fn signal_pending(&self, mask: u64) -> bool {
        self.with_pending_signal(mask, || ()).is_some()
    }
    fn signal_source(&self) -> Option<Arc<crate::file::SignalWaitSource>> {
        if self.is_kernel_thread() {
            return None;
        }
        let state = self.process().signal_state.lock();
        let handlers = state.handlers.as_ref()?;
        let source = handlers.lock().wait_source.clone();
        Some(source)
    }
}

impl TaskControlBlock {
    /// signalfd 与 sigwait 共用真实 dequeue；失效 timer 在 owner 内跳过。
    pub(crate) fn read_signal_notification(&self, mask: u64) -> Option<[u8; 128]> {
        self.take_pending_signal(mask)
            .map(|(signal, info)| info.encode_signalfd(signal))
    }
}

impl TaskControlBlock {
    /// 与 SIG_IGN/NOCLD transition 同一 owner 事务完成 policy 和 pending 发布。
    /// 不能缓存 notify 后到锁外重新进入 generation，否则 IGN 清队列之后会重插入旧事件。
    pub(in crate::task) fn publish_child_signal(
        &self,
        info: super::PendingSignal,
        kind: ChildSignalKind,
        signal: usize,
        prepared: &mut PreparedSignalRecord,
    ) -> Option<ChildSignalPublication> {
        let mask = self.thread().signal_mask.lock();
        self.process()
            .signal_state
            .lock()
            .with_handlers(|handlers, pending| {
                let policy = handlers.child_policy();
                let ordinary = signal == syscall_abi::signal::SIGCHLD;
                let notify = signal != 0
                    && (!ordinary
                        || match kind {
                            ChildSignalKind::Exit => policy.notify_exit,
                            ChildSignalKind::Job => policy.notify_job,
                        });
                let queued = notify
                    && !handlers.ignores_generation(
                        signal,
                        *mask,
                        self.is_namespace_init(),
                        self.tgid() == 1,
                        false,
                    );
                if queued {
                    pending.queue(signal, info, prepared);
                }
                ChildSignalPublication {
                    autoreap: ordinary && policy.autoreap,
                    queued,
                }
            })
    }

    pub(super) fn clone_signal_handlers(
        &self,
        share: bool,
        clear: bool,
    ) -> Result<Arc<Mutex<SignalHandlers>>, MemoryError> {
        let mut snapshot = {
            let state = self.process().signal_state.lock();
            let owner = state
                .handlers
                .as_ref()
                .expect("live Process requires sighand");
            if share {
                return Ok(owner.clone());
            }
            owner.lock().actions
        };
        if clear {
            for action in &mut snapshot {
                if action.handler != 1 {
                    *action = SignalAction::default();
                }
            }
        }
        ProcessSignalState::try_handlers(snapshot)
    }

    /// de_thread 后预留共享表的 destination；唯一表保留 identity 且不分配。
    pub(super) fn prepare_exec_signal_handlers(
        &self,
    ) -> Result<Option<Arc<Mutex<SignalHandlers>>>, MemoryError> {
        {
            let state = self.process().signal_state.lock();
            let owner = state
                .handlers
                .as_ref()
                .expect("live Process requires sighand");
            if Arc::strong_count(owner) == 1 {
                return Ok(None);
            }
        }
        // preparation 只预留 destination；旧表在 commit 前仍是唯一 live disposition。
        // 若提前 snapshot，外部共享 Process 的 IGN transition 可影响本组 generation，却被
        // 最后的旧 snapshot 覆盖。commit 必须在同锁读取最新表并脱离，形成一个线性化点。
        ProcessSignalState::try_default_handlers().map(Some)
    }

    pub(super) fn commit_exec_signal_handlers(&self, prepared: Option<Arc<Mutex<SignalHandlers>>>) {
        let retired = {
            let mut state = self.process().signal_state.lock();
            if let Some(prepared) = prepared {
                {
                    let owner = state.handlers.as_ref().expect("exec requires live sighand");
                    let original = owner.lock();
                    let mut detached = prepared.lock();
                    detached.actions = original.actions;
                    detached.reset_for_exec();
                }
                state.handlers.replace(prepared)
            } else {
                state
                    .with_handlers(|handlers, _| handlers.reset_for_exec())
                    .expect("exec requires live sighand");
                None
            }
        };
        drop(retired);
    }

    /// 仅最后一个 Thread 退出调用；graph/state guard 之外析构最后一份表。
    pub(in crate::task) fn retire_signal_handlers(&self) {
        let retired = {
            let mut state = self.process().signal_state.lock();
            (state.handlers.take(), state.pending.discard(u64::MAX))
        };
        drop(retired);
    }

    /// Graph 冻结同组全部 live Thread；忽略 transition 与 pending 丢弃在同一锁事务。
    pub(in crate::task) fn signal_action_for_threads<'a>(
        &self,
        threads: impl Iterator<Item = &'a Arc<TaskControlBlock>>,
        signal: usize,
        replacement: Option<SignalAction>,
    ) -> Result<(SignalAction, RetiredSignals), ()> {
        self.process()
            .signal_state
            .lock()
            .with_handlers(|handlers, pending| {
                let old = handlers.action(signal);
                let mut retired = RetiredSignals::default();
                if let Some(mut action) = replacement {
                    action.mask = normalize_signal_mask(action.mask);
                    action.flags &= crate::arch::user::SignalActionBytes::supported_flags();
                    handlers.replace(signal, action);
                    if signal_is_ignored(signal, action) {
                        let bit = 1u64 << (signal - 1);
                        retired.append(pending.discard(bit));
                        for thread in threads {
                            retired.append(thread.thread().pending_signals.lock().discard(bit));
                        }
                    }
                }
                (old, retired)
            })
            .ok_or(())
    }
}
