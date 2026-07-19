use crate::ipc::{Pipe, PipeDirection};
use alloc::sync::Arc;

/// @description epoll persistent source index 使用的 domain-neutral readiness identity。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ReadinessSource {
    Console,
    Pipe {
        identity: usize,
        direction: PipeDirection,
    },
}

impl ReadinessSource {
    pub(crate) fn pipe(pipe: &Arc<Pipe>, direction: PipeDirection) -> Self {
        Self::Pipe {
            identity: Pipe::identity(pipe),
            direction,
        }
    }
}

/// @description 一个 OFD interest 的固定上限 source projection；构造与遍历均不分配。
#[derive(Debug, Clone, Copy)]
pub(crate) struct ReadinessSources {
    entries: [Option<ReadinessSource>; 2],
}

impl ReadinessSources {
    pub(crate) const fn new() -> Self {
        Self {
            entries: [None, None],
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
