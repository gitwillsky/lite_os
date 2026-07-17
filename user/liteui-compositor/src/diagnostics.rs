const WIRE_BYTES: usize = 160;

#[derive(Clone, Copy)]
pub(super) struct FrameMetrics {
    pub(super) clips: u32,
    pub(super) pixels: u64,
}

/// Reactor-owned runtime evidence. The hot path only updates fixed scalars;
/// encoding happens solely when a privileged observer requests a snapshot.
pub(super) struct Diagnostics {
    sequence: u64,
    pointer_reports: u64,
    frames_submitted: u64,
    frames_completed: u64,
    frames_retried: u64,
    pointer_samples: u64,
    max_pointer_latency_ms: u64,
    last_pointer_latency_ms: u64,
    max_pointer_pixels: u64,
    last_pointer_pixels: u64,
    clips_submitted: u64,
    pixels_submitted: u64,
    resize_notices: u64,
    resize_attempts: u64,
    resize_commits: u64,
    resize_transient: u64,
    resize_rejected: u64,
    pending_pointer_ms: Option<u64>,
    inflight_pointer_ms: Option<u64>,
    inflight_pointer_pixels: u64,
}

impl Diagnostics {
    pub(super) const fn new() -> Self {
        Self {
            sequence: 0,
            pointer_reports: 0,
            frames_submitted: 0,
            frames_completed: 0,
            frames_retried: 0,
            pointer_samples: 0,
            max_pointer_latency_ms: 0,
            last_pointer_latency_ms: 0,
            max_pointer_pixels: 0,
            last_pointer_pixels: 0,
            clips_submitted: 0,
            pixels_submitted: 0,
            resize_notices: 0,
            resize_attempts: 0,
            resize_commits: 0,
            resize_transient: 0,
            resize_rejected: 0,
            pending_pointer_ms: None,
            inflight_pointer_ms: None,
            inflight_pointer_pixels: 0,
        }
    }

    pub(super) fn pointer_input(&mut self, since_ms: u64) {
        self.pointer_reports = self.pointer_reports.saturating_add(1);
        merge_oldest(&mut self.pending_pointer_ms, since_ms);
    }

    pub(super) fn frame_submitted(&mut self, metrics: FrameMetrics) -> Result<(), ()> {
        if self.inflight_pointer_ms.is_some() {
            return Err(());
        }
        self.frames_submitted = self.frames_submitted.saturating_add(1);
        self.clips_submitted = self
            .clips_submitted
            .saturating_add(u64::from(metrics.clips));
        self.pixels_submitted = self.pixels_submitted.saturating_add(metrics.pixels);
        self.inflight_pointer_ms = self.pending_pointer_ms.take();
        self.inflight_pointer_pixels = if self.inflight_pointer_ms.is_some() {
            metrics.pixels
        } else {
            0
        };
        Ok(())
    }

    pub(super) fn frame_completed(&mut self, now_ms: u64, displayed: bool) {
        if displayed {
            self.frames_completed = self.frames_completed.saturating_add(1);
            self.retire_pointer(now_ms);
        } else {
            self.frames_retried = self.frames_retried.saturating_add(1);
            if let Some(since) = self.inflight_pointer_ms.take() {
                merge_oldest(&mut self.pending_pointer_ms, since);
            }
            self.inflight_pointer_pixels = 0;
        }
    }

    pub(super) fn resize_notice(&mut self) {
        self.resize_notices = self.resize_notices.saturating_add(1);
    }

    pub(super) fn resize_attempt(&mut self) {
        self.resize_attempts = self.resize_attempts.saturating_add(1);
    }

    pub(super) fn resize_commit(&mut self, now_ms: u64) {
        self.resize_commits = self.resize_commits.saturating_add(1);
        if let Some(since) = self.pending_pointer_ms.take() {
            self.record_pointer_sample(now_ms, since, 0);
        }
    }

    pub(super) fn resize_transient(&mut self) {
        self.resize_transient = self.resize_transient.saturating_add(1);
    }

    pub(super) fn resize_rejected(&mut self) {
        self.resize_rejected = self.resize_rejected.saturating_add(1);
    }

    pub(super) fn snapshot(
        &mut self,
        width: usize,
        height: usize,
        damage_pending: bool,
        preview_active: bool,
        client_mask: u32,
    ) -> [u8; WIRE_BYTES] {
        self.sequence = self.sequence.saturating_add(1);
        let mut wire = [0u8; WIRE_BYTES];
        wire[..4].copy_from_slice(b"LUD1");
        put_u16(&mut wire, 4, 1);
        put_u16(&mut wire, 6, WIRE_BYTES as u16);
        for (offset, value) in [
            self.sequence,
            self.pointer_reports,
            self.frames_submitted,
            self.frames_completed,
            self.frames_retried,
            self.pointer_samples,
            self.max_pointer_latency_ms,
            self.last_pointer_latency_ms,
            self.max_pointer_pixels,
            self.last_pointer_pixels,
            self.clips_submitted,
            self.pixels_submitted,
            self.resize_notices,
            self.resize_attempts,
            self.resize_commits,
            self.resize_transient,
            self.resize_rejected,
        ]
        .into_iter()
        .enumerate()
        {
            put_u64(&mut wire, 8 + offset * 8, value);
        }
        put_u32(&mut wire, 144, width.try_into().unwrap_or(u32::MAX));
        put_u32(&mut wire, 148, height.try_into().unwrap_or(u32::MAX));
        let flags = u32::from(self.pending_pointer_ms.is_some())
            | u32::from(self.inflight_pointer_ms.is_some()) << 1
            | u32::from(damage_pending) << 2
            | u32::from(preview_active) << 3;
        put_u32(&mut wire, 152, flags);
        put_u32(&mut wire, 156, client_mask);
        wire
    }

    fn retire_pointer(&mut self, now_ms: u64) {
        if let Some(since) = self.inflight_pointer_ms.take() {
            self.record_pointer_sample(now_ms, since, self.inflight_pointer_pixels);
        }
        self.inflight_pointer_pixels = 0;
    }

    fn record_pointer_sample(&mut self, now_ms: u64, since_ms: u64, pixels: u64) {
        self.pointer_samples = self.pointer_samples.saturating_add(1);
        self.last_pointer_latency_ms = now_ms.saturating_sub(since_ms);
        self.max_pointer_latency_ms = self
            .max_pointer_latency_ms
            .max(self.last_pointer_latency_ms);
        self.last_pointer_pixels = pixels;
        self.max_pointer_pixels = self.max_pointer_pixels.max(pixels);
    }
}

fn merge_oldest(target: &mut Option<u64>, value: u64) {
    *target = Some(target.map_or(value, |current| current.min(value)));
}

fn put_u16(output: &mut [u8], offset: usize, value: u16) {
    output[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(output: &mut [u8], offset: usize, value: u32) {
    output[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u64(output: &mut [u8], offset: usize, value: u64) {
    output[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}
