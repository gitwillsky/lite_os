use crate::ipc::{Pipe, PipeDirection};
use alloc::sync::Arc;

/// epoll persistent source index 使用的 domain-neutral readiness identity。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ReadinessSource {
    Console,
    Signal {
        identity: usize,
    },
    Pipe {
        identity: usize,
        direction: PipeDirection,
    },
}

/// Deferred source callback 没有用户 reader；signalfd 必须保守入队后由实际 caller 复查。
pub(super) struct SourceNotificationContext;
impl super::FileReadinessContext for SourceNotificationContext {
    fn signal_pending(&self, _: u64) -> bool {
        false
    }
    fn signal_source(&self) -> Option<Arc<super::SignalWaitSource>> {
        None
    }
}

impl ReadinessSource {
    pub(crate) fn pipe(pipe: &Arc<Pipe>, direction: PipeDirection) -> Self {
        Self::Pipe {
            identity: Pipe::identity(pipe),
            direction,
        }
    }
}

/// 一个 OFD interest 的固定上限 source projection；构造与遍历均不分配。
#[derive(Debug, Clone, Copy)]
pub(crate) struct ReadinessSources {
    entries: [Option<ReadinessSource>; 3],
}

impl ReadinessSources {
    pub(crate) const fn new() -> Self {
        Self {
            entries: [None, None, None],
        }
    }

    pub(crate) fn push(&mut self, source: ReadinessSource) {
        if self.entries.contains(&Some(source)) {
            return;
        }
        let entry = self
            .entries
            .iter_mut()
            .find(|entry| entry.is_none())
            .expect("OFD readiness source projection exceeded fixed bound");
        *entry = Some(source);
    }

    pub(crate) fn iter(self) -> impl Iterator<Item = ReadinessSource> {
        self.entries.into_iter().flatten()
    }
}
