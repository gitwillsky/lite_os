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
    /// @return 共享 eventfd owner；control block 分配失败返回空错误。
    pub(crate) fn new(
        initial: u64,
        semaphore: bool,
        read_pair: (Arc<PipeEnd>, Arc<PipeEnd>),
        write_pair: (Arc<PipeEnd>, Arc<PipeEnd>),
    ) -> Result<Arc<Self>, ()> {
        Arc::try_new(Self {
            counter: Mutex::new(initial),
            semaphore,
            read_notify: read_pair.0,
            read_signal: read_pair.1,
            write_notify: write_pair.0,
            write_signal: write_pair.1,
        })
        .map_err(|_| ())
    }

    pub(crate) fn read(&self) -> EventFdRead {
        let result = {
            let mut counter = self.counter.lock();
            if *counter == 0 {
                return EventFdRead::Empty;
            }
            let value = if self.semaphore { 1 } else { *counter };
            *counter -= value;
            if *counter == 0 {
                self.read_notify.drain_readiness();
            }
            value
        };
        // Linux eventfd 对每次成功 read 都发布 EPOLLOUT wake；只在 full→writable
        // 边界通知会让 EPOLLET consumer 丢失后续 read 产生的独立 edge。
        self.write_signal.signal_readiness();
        EventFdRead::Value(result)
    }

    pub(crate) fn write(&self, value: u64) -> EventFdWrite {
        {
            let mut counter = self.counter.lock();
            if value > MAX_COUNTER - *counter {
                return EventFdWrite::Full;
            }
            *counter += value;
            if *counter == MAX_COUNTER {
                self.write_notify.drain_readiness();
            }
        }
        // Linux eventfd 对每次成功 write 都发布 EPOLLIN wake。Mio 的 eventfd
        // waker 在 epoll backend 下不读取 counter；缺少重复 edge 会让 Tokio task
        // 已进入 inject queue，但所有 worker 仍永久停在 parker。
        self.read_signal.signal_readiness();
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::{Pipe, PipeNotifier};

    struct TestNotifier;

    impl PipeNotifier for TestNotifier {
        fn notify(&self, _pipe: &Arc<Pipe>) {}
    }

    fn eventfd(initial: u64, semaphore: bool) -> Arc<EventFd> {
        let notifier: Arc<dyn PipeNotifier> = Arc::new(TestNotifier);
        let read_pair = Pipe::notification_pair(notifier.clone()).expect("read notification pair");
        let write_pair = Pipe::notification_pair(notifier).expect("write notification pair");
        EventFd::new(initial, semaphore, read_pair, write_pair).expect("eventfd")
    }

    #[test]
    fn every_successful_write_publishes_a_read_edge() {
        let event = eventfd(0, false);
        let initial = event.readiness_generation(0x001);

        assert_eq!(event.write(1), EventFdWrite::Written);
        let first = event.readiness_generation(0x001);
        assert_ne!(first, initial);

        assert_eq!(event.write(1), EventFdWrite::Written);
        let second = event.readiness_generation(0x001);
        assert_ne!(second, first);

        assert_eq!(event.write(0), EventFdWrite::Written);
        assert_ne!(event.readiness_generation(0x001), second);
    }

    #[test]
    fn every_successful_read_publishes_a_write_edge() {
        let event = eventfd(2, true);
        let initial = event.readiness_generation(0x004);

        assert_eq!(event.read(), EventFdRead::Value(1));
        let first = event.readiness_generation(0x004);
        assert_ne!(first, initial);

        assert_eq!(event.read(), EventFdRead::Value(1));
        assert_ne!(event.readiness_generation(0x004), first);
    }
}
