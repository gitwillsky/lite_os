use std::sync::atomic::Ordering;

use audio_proto::MAX_SESSION_STREAMS;

use super::{HISTORY_PERIODS, Mixer, MixerEvent, PROGRESS_EVENT_FRAMES, PlaybackDevice};

#[derive(Clone, Copy)]
pub(super) struct PeriodHistory {
    pub(super) submitted_end: u64,
    pub(super) counts: [u16; MAX_SESSION_STREAMS],
}

impl PeriodHistory {
    pub(super) const EMPTY: Self = Self {
        submitted_end: 0,
        counts: [0; MAX_SESSION_STREAMS],
    };
}

impl<D: PlaybackDevice> Mixer<D> {
    pub(super) fn push_history(&mut self, history: PeriodHistory) {
        let next = (self.history_head + 1) % HISTORY_PERIODS;
        if next == self.history_tail {
            self.failed.store(true, Ordering::Release);
            return;
        }
        self.history[self.history_head] = history;
        self.history_head = next;
    }

    pub(super) fn confirm_history(&mut self, confirmed_device_frame: u64) {
        while self.history_tail != self.history_head {
            let history = self.history[self.history_tail];
            if history.submitted_end > confirmed_device_frame {
                break;
            }
            for (slot, count) in history.counts.into_iter().enumerate() {
                if count == 0 {
                    continue;
                }
                let Some(stream) = self.streams[slot].as_mut() else {
                    continue;
                };
                stream.confirmed_frames = stream.confirmed_frames.saturating_add(u64::from(count));
                let drained = stream
                    .empty_confirmation_target
                    .is_some_and(|target| stream.confirmed_frames >= target);
                if drained
                    || stream
                        .confirmed_frames
                        .saturating_sub(stream.reported_frames)
                        >= PROGRESS_EVENT_FRAMES
                {
                    if drained {
                        stream.empty_confirmation_target = None;
                    }
                    stream.reported_frames = stream.confirmed_frames;
                    let event = MixerEvent::Progress {
                        stream_id: stream.id,
                        generation: stream.generation,
                        consumed_frames: stream.confirmed_frames,
                    };
                    self.publish(event);
                }
            }
            self.history_tail = (self.history_tail + 1) % HISTORY_PERIODS;
        }
    }
}
