use alloc::sync::Arc;

use super::DrmFile;
use crate::ipc::{Pipe, PipeDirection};

pub(super) const EVENT_QUEUE_CAPACITY: usize = 4096 / DrmEvent::SIZE;

/// @description 一个 Linux RV64 `drm_event_vblank` page-flip completion 值。
#[derive(Clone, Copy)]
pub(crate) struct DrmEvent {
    pub(super) user_data: u64,
    pub(super) seconds: u32,
    pub(super) microseconds: u32,
    pub(super) sequence: u32,
}

impl DrmEvent {
    pub(crate) const SIZE: usize = 32;
    pub(crate) const EMPTY: Self = Self {
        user_data: 0,
        seconds: 0,
        microseconds: 0,
        sequence: 0,
    };

    /// @description 编码 Linux RV64 `drm_event_vblank` page-flip completion。
    /// @return 32-byte native-endian `DRM_EVENT_FLIP_COMPLETE` payload。
    pub(crate) fn encode(self) -> [u8; Self::SIZE] {
        let mut bytes = [0u8; Self::SIZE];
        bytes[0..4].copy_from_slice(&2u32.to_ne_bytes());
        bytes[4..8].copy_from_slice(&(Self::SIZE as u32).to_ne_bytes());
        bytes[8..16].copy_from_slice(&self.user_data.to_ne_bytes());
        bytes[16..20].copy_from_slice(&self.seconds.to_ne_bytes());
        bytes[20..24].copy_from_slice(&self.microseconds.to_ne_bytes());
        bytes[24..28].copy_from_slice(&self.sequence.to_ne_bytes());
        bytes[28..32].copy_from_slice(&1u32.to_ne_bytes());
        bytes
    }
}

pub(super) struct EventQueue {
    events: [DrmEvent; EVENT_QUEUE_CAPACITY],
    head: usize,
    length: usize,
}

impl EventQueue {
    pub(super) fn new() -> Self {
        Self {
            events: [DrmEvent::EMPTY; EVENT_QUEUE_CAPACITY],
            head: 0,
            length: 0,
        }
    }

    pub(super) fn len(&self) -> usize {
        self.length
    }

    pub(super) fn push(&mut self, event: DrmEvent) {
        assert!(self.length < self.events.len());
        let tail = (self.head + self.length) % self.events.len();
        self.events[tail] = event;
        self.length += 1;
    }

    fn read(&mut self, output: &mut [DrmEvent]) -> usize {
        let count = output.len().min(self.length);
        for event in output.iter_mut().take(count) {
            *event = self.events[self.head];
            self.head = (self.head + 1) % self.events.len();
            self.length -= 1;
        }
        count
    }
}

impl DrmFile {
    /// @description 返回当前 OFD 已排队的完整 DRM event 数。
    /// @return 零表示 read 必须阻塞或返回 EAGAIN。
    pub(crate) fn readable_event_count(&self) -> usize {
        self.events.lock().len()
    }

    /// @description 原子消费不超过 output 长度的完整 DRM events。
    /// @param output kernel stack staging event slice。
    /// @return 实际消费的 event 数。
    pub(crate) fn read_events(&self, output: &mut [DrmEvent]) -> usize {
        self.events.lock().read(output)
    }

    /// @description 排空旧 completion token 后复查当前 OFD event level readiness。
    /// @return 仍需阻塞时返回共享 device Pipe；已有 event 返回 None。
    pub(crate) fn prepare_to_block(&self) -> Option<Arc<Pipe>> {
        if self.readable_event_count() != 0 {
            return None;
        }
        self.device.completion_read.drain_readiness();
        (self.readable_event_count() == 0).then(|| self.device.completion_read.pipe())
    }

    /// @description 返回 DRM event wait source 的单调 generation。
    /// @return 可供 epoll edge-triggered 比较的 generation。
    pub(crate) fn readiness_generation(&self) -> u64 {
        self.device
            .completion_read
            .pipe()
            .readiness_generation(PipeDirection::Read)
    }

    /// @description 取得 poll registration 使用的共享 completion notification Pipe。
    /// @return device read-side Pipe Arc。
    pub(crate) fn notification_pipe(&self) -> Arc<Pipe> {
        self.device.completion_read.pipe()
    }
}
