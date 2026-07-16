use crate::drivers::RawInputEvent;

use super::{EV_SYN, InputEvent, SYN_DROPPED, SYN_REPORT};

const CLIENT_BUFFER_SIZE: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum InputClock {
    Realtime,
    Monotonic,
    Boottime,
}

#[derive(Clone, Copy)]
pub(super) struct EventTimes {
    pub(super) realtime_ns: u64,
    pub(super) monotonic_ns: u64,
}

/// @description 单 evdev OFD 的有界 packet ring 与 timestamp clock owner。
pub(super) struct ClientQueue {
    buffer: [InputEvent; CLIENT_BUFFER_SIZE],
    head: usize,
    tail: usize,
    packet_head: usize,
    clock: InputClock,
}

impl ClientQueue {
    pub(super) fn new() -> Self {
        Self {
            buffer: [InputEvent::default(); CLIENT_BUFFER_SIZE],
            head: 0,
            tail: 0,
            packet_head: 0,
            clock: InputClock::Realtime,
        }
    }

    fn readable(&self) -> bool {
        self.packet_head != self.tail
    }

    pub(super) fn readable_count(&self) -> usize {
        self.packet_head
            .wrapping_sub(self.tail)
            .wrapping_add(CLIENT_BUFFER_SIZE)
            % CLIENT_BUFFER_SIZE
    }

    fn timestamp(&self, times: EventTimes) -> (i64, i64) {
        let nanoseconds = match self.clock {
            InputClock::Realtime => times.realtime_ns,
            // LiteOS 尚无 suspend domain；CLOCK_BOOTTIME 与 CLOCK_MONOTONIC 同源且不会漂移。
            InputClock::Monotonic | InputClock::Boottime => times.monotonic_ns,
        };
        (
            (nanoseconds / 1_000_000_000) as i64,
            (nanoseconds % 1_000_000_000 / 1_000) as i64,
        )
    }

    pub(super) fn pass(&mut self, raw: RawInputEvent, times: EventTimes) -> bool {
        let was_readable = self.readable();
        if raw.event_type == EV_SYN && raw.code == SYN_REPORT && self.packet_head == self.head {
            return false;
        }
        let (seconds, microseconds) = self.timestamp(times);
        let event = InputEvent {
            seconds,
            microseconds,
            event_type: raw.event_type,
            code: raw.code,
            value: raw.value,
        };
        self.buffer[self.head] = event;
        self.head = (self.head + 1) & (CLIENT_BUFFER_SIZE - 1);
        if self.head == self.tail {
            // 与 Linux evdev 相同：保留 SYN_DROPPED 和最新事件，packet_head 停在 dropped；
            // 直到下一 SYN_REPORT 到达前 read/poll 都不得暴露不完整 packet。
            self.tail = (self.head + CLIENT_BUFFER_SIZE - 2) & (CLIENT_BUFFER_SIZE - 1);
            self.buffer[self.tail] = InputEvent {
                code: SYN_DROPPED,
                event_type: EV_SYN,
                value: 0,
                seconds,
                microseconds,
            };
            self.packet_head = self.tail;
        }
        if raw.event_type == EV_SYN && raw.code == SYN_REPORT {
            self.packet_head = self.head;
        }
        !was_readable && self.readable()
    }

    pub(super) fn read(&mut self, output: &mut [InputEvent]) -> usize {
        let count = output.len().min(self.readable_count());
        for event in output.iter_mut().take(count) {
            *event = self.buffer[self.tail];
            self.tail = (self.tail + 1) & (CLIENT_BUFFER_SIZE - 1);
        }
        count
    }

    pub(super) fn flush_type(&mut self, event_type: u16) {
        debug_assert_ne!(event_type, EV_SYN);
        let old_head = self.head;
        let mut source = self.tail;
        let mut destination = self.tail;
        self.packet_head = self.tail;
        // Linux 保留 leading SYN_REPORT，因此从 1 开始；删除某 type 后，空 packet 的
        // SYN_REPORT 也必须删除，否则 userspace 会观察到没有状态变化的伪 packet。
        let mut packet_entries = 1usize;
        while source != old_head {
            let event = self.buffer[source];
            source = (source + 1) & (CLIENT_BUFFER_SIZE - 1);
            let report = event.event_type == EV_SYN && event.code == SYN_REPORT;
            if event.event_type == event_type || (report && packet_entries == 0) {
                continue;
            }
            self.buffer[destination] = event;
            destination = (destination + 1) & (CLIENT_BUFFER_SIZE - 1);
            packet_entries += 1;
            if report {
                packet_entries = 0;
                self.packet_head = destination;
            }
        }
        self.head = destination;
    }

    pub(super) fn set_clock(&mut self, clock: InputClock, times: EventTimes) {
        if self.clock == clock {
            return;
        }
        self.clock = clock;
        if self.head == self.tail {
            return;
        }
        self.head = self.tail;
        self.packet_head = self.tail;
        let (seconds, microseconds) = self.timestamp(times);
        self.buffer[self.head] = InputEvent {
            seconds,
            microseconds,
            event_type: EV_SYN,
            code: SYN_DROPPED,
            value: 0,
        };
        self.head = (self.head + 1) & (CLIENT_BUFFER_SIZE - 1);
    }
}
