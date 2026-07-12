use alloc::{sync::Arc, vec::Vec};

use crate::ipc::{Pipe, PipeDirection};

/// @description IndexedWaitQueue entry 的唯一 wait kind discriminator。
#[derive(Clone, Copy)]
pub(super) enum IndexedWaitKind {
    Deadline,
    Futex {
        tgid: usize,
        address: usize,
    },
    Console,
    Signal {
        mask: u64,
    },
    Pipe {
        identity: usize,
        direction: PipeDirection,
    },
    Poll,
}

/// @description 一次 Poll membership 在具体 source 上的 registration mode。
/// wake_group 是 epoll instance identity；缺失时同一 epoll 上多个 epoll_wait thread 会被
/// 当成多个 target callback，破坏 Linux 每个 eventpoll instance 只唤醒一个 waiter 的层次。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum PollWaitKey {
    Console {
        events: i16,
        exclusive: bool,
        wake_group: Option<usize>,
    },
    Pipe {
        identity: usize,
        direction: PipeDirection,
        events: i16,
        exclusive: bool,
        wake_group: Option<usize>,
    },
}

impl PollWaitKey {
    /// @description 构造 console source registration。
    ///
    /// @param events 可消费 console wake 的 poll mask。
    /// @param exclusive true 表示本次 source wake 最多选择一个同类 waiter。
    /// @param wake_group epoll waiter 使用 instance identity；ppoll/direct wait 使用 None。
    /// @return console wait key。
    pub(crate) fn console(events: i16, exclusive: bool, wake_group: Option<usize>) -> Self {
        Self::Console {
            events,
            exclusive,
            wake_group,
        }
    }

    /// @description 构造 Pipe source registration。
    ///
    /// @param pipe Pipe owner identity。
    /// @param direction read/write readiness direction。
    /// @param events 该 source 上可消费 wake 的 poll mask。
    /// @param exclusive true 表示本次 source wake 最多选择一个同类 waiter。
    /// @param wake_group epoll waiter 使用 instance identity；ppoll/direct wait 使用 None。
    /// @return Pipe wait key。
    pub(crate) fn pipe(
        pipe: &Arc<Pipe>,
        direction: PipeDirection,
        events: i16,
        exclusive: bool,
        wake_group: Option<usize>,
    ) -> Self {
        Self::Pipe {
            identity: Pipe::identity(pipe),
            direction,
            events,
            exclusive,
            wake_group,
        }
    }

    /// @description 查询该 source registration 是否参与 wake-one 选择。
    ///
    /// @return exclusive registration 返回 true。
    pub(super) fn exclusive(self) -> bool {
        match self {
            Self::Console { exclusive, .. } | Self::Pipe { exclusive, .. } => exclusive,
        }
    }

    fn same_source(self, other: Self) -> bool {
        match (self, other) {
            (Self::Console { .. }, Self::Console { .. }) => true,
            (
                Self::Pipe {
                    identity: left_identity,
                    direction: left_direction,
                    ..
                },
                Self::Pipe {
                    identity: right_identity,
                    direction: right_direction,
                    ..
                },
            ) => left_identity == right_identity && left_direction == right_direction,
            _ => false,
        }
    }

    fn with_exclusive(self, exclusive: bool) -> Self {
        match self {
            Self::Console {
                events, wake_group, ..
            } => Self::Console {
                events,
                exclusive,
                wake_group,
            },
            Self::Pipe {
                identity,
                direction,
                events,
                wake_group,
                ..
            } => Self::Pipe {
                identity,
                direction,
                events,
                exclusive,
                wake_group,
            },
        }
    }

    fn events(self) -> i16 {
        match self {
            Self::Console { events, .. } | Self::Pipe { events, .. } => events,
        }
    }

    fn with_events(self, events: i16) -> Self {
        match self {
            Self::Console {
                exclusive,
                wake_group,
                ..
            } => Self::Console {
                events,
                exclusive,
                wake_group,
            },
            Self::Pipe {
                identity,
                direction,
                exclusive,
                wake_group,
                ..
            } => Self::Pipe {
                identity,
                direction,
                events,
                exclusive,
                wake_group,
            },
        }
    }

    /// @description 判断 console wake mask 是否匹配该 key。
    pub(super) fn matches_console(self, ready: i16) -> bool {
        matches!(self, Self::Console { .. }) && self.events() & ready != 0
    }

    /// @description 返回用于同一 source wake 去重 epoll_wait thread 的 instance identity。
    pub(super) fn wake_group(self) -> Option<usize> {
        match self {
            Self::Console { wake_group, .. } | Self::Pipe { wake_group, .. } => wake_group,
        }
    }

    /// @description 判断 Pipe identity/direction/wake mask 是否匹配该 key。
    pub(super) fn matches_pipe(
        self,
        identity: usize,
        direction: PipeDirection,
        ready: i16,
    ) -> bool {
        matches!(
            self,
            Self::Pipe {
                identity: candidate,
                direction: candidate_direction,
                ..
            } if candidate == identity && candidate_direction == direction
        ) && self.events() & ready != 0
    }

    /// @description 就地合并同一 source 的重复 key，普通 registration 优先于 exclusive。
    ///
    /// @param keys 一个 Poll membership 的未规范化 keys。
    pub(super) fn normalize(keys: &mut Vec<Self>) {
        keys.sort_unstable();
        keys.dedup_by(|later, earlier| {
            if !earlier.same_source(*later) {
                return false;
            }
            *earlier = earlier
                .with_events(earlier.events() | later.events())
                .with_exclusive(earlier.exclusive() && later.exclusive());
            true
        });
    }
}
