use alloc::sync::Arc;
use spin::Mutex;

use super::{Pipe, PipeEnd};

const MAX_COUNTER: u64 = u64::MAX - 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EventFdRead {
    Value(u64),
    Empty,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EventFdWrite {
    Written,
    Full,
}

/// @description Linux eventfd 的唯一 64-bit counter owner 与 readiness source。
pub(crate) struct EventFd {
    counter: Mutex<u64>,
    semaphore: bool,
    read_notify: Arc<PipeEnd>,
    read_signal: Arc<PipeEnd>,
    write_notify: Arc<PipeEnd>,
    write_signal: Arc<PipeEnd>,
}

impl EventFd {
    /// @description 从两对 notification Pipe 构造 eventfd；counter 不复制到其他 owner。
    /// @param initial 初始 counter。
    /// @param semaphore EFD_SEMAPHORE read 是否每次只消费一。
    /// @param read_pair readable edge 的 read/write notification endpoints。
    /// @param write_pair writable edge 的 read/write notification endpoints。
    /// @return 共享 eventfd owner。
    pub(crate) fn new(
        initial: u64,
        semaphore: bool,
        read_pair: (Arc<PipeEnd>, Arc<PipeEnd>),
        write_pair: (Arc<PipeEnd>, Arc<PipeEnd>),
    ) -> Arc<Self> {
        Arc::new(Self {
            counter: Mutex::new(initial),
            semaphore,
            read_notify: read_pair.0,
            read_signal: read_pair.1,
            write_notify: write_pair.0,
            write_signal: write_pair.1,
        })
    }

    pub(crate) fn read(&self) -> EventFdRead {
        let (result, became_writable) = {
            let mut counter = self.counter.lock();
            if *counter == 0 {
                return EventFdRead::Empty;
            }
            let became_writable = *counter == MAX_COUNTER;
            let value = if self.semaphore { 1 } else { *counter };
            *counter -= value;
            if *counter == 0 {
                self.read_notify.drain_readiness();
            }
            (value, became_writable)
        };
        if became_writable {
            self.write_signal.signal_readiness();
        }
        EventFdRead::Value(result)
    }

    pub(crate) fn write(&self, value: u64) -> EventFdWrite {
        if value == 0 {
            return EventFdWrite::Written;
        }
        let became_readable = {
            let mut counter = self.counter.lock();
            if value > MAX_COUNTER - *counter {
                return EventFdWrite::Full;
            }
            let became_readable = *counter == 0;
            *counter += value;
            if *counter == MAX_COUNTER {
                self.write_notify.drain_readiness();
            }
            became_readable
        };
        if became_readable {
            self.read_signal.signal_readiness();
        }
        EventFdWrite::Written
    }

    pub(crate) fn readable(&self) -> bool {
        *self.counter.lock() != 0
    }

    pub(crate) fn writable(&self) -> bool {
        *self.counter.lock() != MAX_COUNTER
    }

    pub(crate) fn notification_pipe(&self, read: bool) -> Arc<Pipe> {
        if read {
            self.read_notify.pipe()
        } else {
            self.write_notify.pipe()
        }
    }

    /// @description 投影调用者关心方向的最新 readiness generation。
    /// @param events Linux poll event mask；同时关心读写时返回两者较新值。
    /// @return 可用于 edge-triggered 变更检测的单调 generation。
    pub(crate) fn readiness_generation(&self, events: i16) -> u64 {
        let mut generation = 0;
        if events & 0x001 != 0 {
            generation = self
                .read_notify
                .pipe()
                .readiness_generation(super::PipeDirection::Read);
        }
        if events & 0x004 != 0 {
            generation = generation.max(
                self.write_notify
                    .pipe()
                    .readiness_generation(super::PipeDirection::Read),
            );
        }
        generation
    }
}
