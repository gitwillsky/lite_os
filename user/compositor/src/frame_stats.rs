//! Guest-vblank frame-timing accumulator for the AArch64+HVF 60 Hz gate.
//!
//! The compositor is the single point where every real page flip completes
//! (`lib.rs` present chokepoint), and the [`FlipEvent`] it receives there
//! carries the kernel monotonic vblank clock (`seconds`/`microseconds`) plus the
//! device presentation `sequence`. Differencing successive vblank timestamps
//! yields the true present-to-present interval — measured on the guest's own
//! monotonic clock, not the Python host wall clock, which is what makes this the
//! one legitimate exception to the repo's rejection of wall-clock timing gates
//! (see `docs/development/build-and-verify.md`).
//!
//! Every field is fixed-capacity: the steady frame path performs no allocation
//! (the LiteUI contract forbids it), and the only non-trivial work — sorting for
//! percentiles — happens once per [`WINDOW`] frames into a preallocated scratch
//! field, off the per-frame path.

use linux_uapi::drm::FlipEvent;

/// Frames accumulated before one `compositor: frame-stats` report is emitted.
///
/// ~8.5 s at 60 Hz — long enough that a single window is a representative
/// steady-state sample, short enough that a driven gate fills one within a
/// ~15 s workload with margin.
pub(crate) const WINDOW: usize = 512;

/// Converts a completed page flip's kernel monotonic vblank time to nanoseconds.
///
/// `seconds` is monotonic seconds and `microseconds` the sub-second remainder;
/// `u64::from(u32) * 1_000_000_000` cannot overflow `u64`. Shared with
/// [`crate::session`] so the ns conversion has one definition.
pub(crate) fn flip_monotonic_ns(event: &FlipEvent) -> u64 {
    u64::from(event.seconds) * 1_000_000_000 + u64::from(event.microseconds) * 1_000
}

/// Rolling window of successive-frame present intervals and dropped-frame count.
pub(crate) struct FrameStats {
    /// Present-to-present intervals in microseconds, indexed `count % WINDOW`.
    intervals_us: [u32; WINDOW],
    /// Sort buffer reused at emit time so percentiles never allocate.
    scratch: [u32; WINDOW],
    /// Intervals recorded since the last arm (also the emit trigger via modulo).
    count: usize,
    /// Previous flip's monotonic ns; `0` means the baseline is unseeded.
    last_ns: u64,
    /// Previous flip's device sequence, for gap-based dropped-frame counting.
    last_sequence: u32,
    /// Summed vblank sequence gaps in the current window (a compositor miss).
    dropped: u32,
    /// Whether the desktop has reached steady state; boot frames are excluded.
    armed: bool,
}

impl FrameStats {
    pub(crate) fn new() -> Self {
        Self {
            intervals_us: [0; WINDOW],
            scratch: [0; WINDOW],
            count: 0,
            last_ns: 0,
            last_sequence: 0,
            dropped: 0,
            armed: false,
        }
    }

    /// Begins steady-state measurement, excluding everything before it.
    ///
    /// Idempotent: only the first call after construction/reset matters. Clearing
    /// the baseline (`last_ns = 0`) means the first post-arm frame only seeds the
    /// predecessor, so the one-off boot→desktop transition interval (a 30 Hz boot
    /// frame followed by the first desktop present) is never recorded.
    pub(crate) fn arm(&mut self) {
        if self.armed {
            return;
        }
        self.armed = true;
        self.last_ns = 0;
        self.last_sequence = 0;
    }

    /// Drops a partially filled window, e.g. when a desktop epoch is torn down.
    pub(crate) fn reset(&mut self) {
        // Emit whatever the current window holds before discarding it, so a run
        // shorter than one full WINDOW still surfaces a (short) report rather
        // than silently producing nothing. The gate's minimum-sample guard
        // decides whether the sample is large enough to trust.
        let filled = self.count % WINDOW;
        if self.armed && filled > 0 {
            self.emit(filled);
        }
        self.count = 0;
        self.last_ns = 0;
        self.last_sequence = 0;
        self.dropped = 0;
        self.armed = false;
    }

    /// Records one completed present; emits a report every [`WINDOW`] frames.
    pub(crate) fn record(&mut self, monotonic_ns: u64, sequence: u32) {
        if !self.armed {
            return;
        }
        if self.last_ns == 0 {
            // First frame after arm has no predecessor: seed the baseline only.
            self.last_ns = monotonic_ns;
            self.last_sequence = sequence;
            return;
        }
        let delta_ns = monotonic_ns.saturating_sub(self.last_ns);
        self.intervals_us[self.count % WINDOW] = (delta_ns / 1_000) as u32;
        // Each skipped device sequence number between two presents is one vblank
        // the compositor failed to flip on — a dropped frame.
        self.dropped += sequence.wrapping_sub(self.last_sequence).saturating_sub(1);
        self.last_ns = monotonic_ns;
        self.last_sequence = sequence;
        self.count += 1;
        if self.count.is_multiple_of(WINDOW) {
            self.emit(WINDOW);
        }
    }

    /// Emits one greppable report line for the most recent `len` intervals.
    ///
    /// Percentiles are nearest-rank over a sorted copy in `self.scratch`; the sort
    /// is the only heavy work and runs once per window, never on the steady path.
    fn emit(&mut self, len: usize) {
        self.scratch[..len].copy_from_slice(&self.intervals_us[..len]);
        self.scratch[..len].sort_unstable();
        let percentile = |rank: usize| -> u32 {
            // Nearest-rank: ceil(p * len) mapped to a 0-based index in [0, len).
            let index = ((rank * len).div_ceil(100)).saturating_sub(1).min(len - 1);
            self.scratch[index]
        };
        let (p50, p95, p99) = (percentile(50), percentile(95), percentile(99));
        eprintln!(
            "compositor: frame-stats window={WINDOW} frames={len} dropped={} \
             p50_us={p50} p95_us={p95} p99_us={p99}",
            self.dropped,
        );
        self.dropped = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::{FrameStats, WINDOW, flip_monotonic_ns};
    use linux_uapi::drm::FlipEvent;

    fn flip(seconds: u32, microseconds: u32, sequence: u32) -> FlipEvent {
        FlipEvent {
            user_data: 0,
            seconds,
            microseconds,
            sequence,
        }
    }

    #[test]
    fn monotonic_ns_combines_seconds_and_micros() {
        assert_eq!(flip_monotonic_ns(&flip(2, 500_000, 0)), 2_500_000_000);
        // Max fields do not overflow u64.
        assert_eq!(
            flip_monotonic_ns(&flip(u32::MAX, 999_999, 0)),
            u64::from(u32::MAX) * 1_000_000_000 + 999_999_000
        );
    }

    #[test]
    fn unarmed_records_nothing() {
        let mut stats = FrameStats::new();
        stats.record(1_000_000, 1);
        stats.record(2_000_000, 2);
        assert_eq!(stats.count, 0);
    }

    #[test]
    fn arm_excludes_the_warmup_boundary_interval() {
        let mut stats = FrameStats::new();
        stats.arm();
        // First post-arm frame only seeds the baseline — no interval recorded.
        stats.record(100_000_000, 10);
        assert_eq!(stats.count, 0);
        assert_eq!(stats.last_ns, 100_000_000);
        // Second frame records the first real interval (16 ms).
        stats.record(116_000_000, 11);
        assert_eq!(stats.count, 1);
        assert_eq!(stats.intervals_us[0], 16_000);
    }

    #[test]
    fn arm_is_idempotent() {
        let mut stats = FrameStats::new();
        stats.arm();
        stats.record(100_000_000, 1); // seeds baseline
        stats.arm(); // must NOT re-clear the baseline mid-window
        assert_eq!(stats.last_ns, 100_000_000);
        stats.record(116_000_000, 2);
        assert_eq!(stats.count, 1);
    }

    #[test]
    fn dropped_counts_sequence_gaps_including_wraparound() {
        let mut stats = FrameStats::new();
        stats.arm();
        stats.record(0, u32::MAX - 1); // seed
        // Contiguous sequence: no drops.
        stats.record(16_000, u32::MAX);
        assert_eq!(stats.dropped, 0);
        // Wrap MAX -> 2 skips sequence 0 and 1 => 2 dropped frames.
        stats.record(32_000, 2);
        assert_eq!(stats.dropped, 2);
    }

    #[test]
    fn emit_fires_exactly_at_window_multiples() {
        let mut stats = FrameStats::new();
        stats.arm();
        // Seed with a nonzero baseline (ns==0 would read as "unseeded"), then
        // WINDOW recorded intervals => exactly one emit; dropped resets.
        stats.record(16_000, 0);
        for i in 1..=WINDOW {
            stats.record(((i + 1) as u64) * 16_000, i as u32);
        }
        assert_eq!(stats.count, WINDOW);
        assert_eq!(stats.dropped, 0); // reset by emit
    }

    #[test]
    fn nearest_rank_percentiles_on_known_intervals() {
        let mut stats = FrameStats::new();
        stats.arm();
        stats.record(1_000, 0); // seed with a nonzero baseline
        // 100 intervals of 1000, 2000, ..., 100000 us (sorted ranks 1..=100).
        for i in 1..=100u64 {
            stats.record(1_000 + cumulative_us(i), i as u32);
        }
        assert_eq!(stats.count, 100);
        stats.emit(100);
        // scratch now holds the sorted first 100 intervals (1000..=100000).
        // Nearest-rank: p50 -> index ceil(0.5*100)-1 = 49 -> 50000;
        // p95 -> index 94 -> 95000; p99 -> index 98 -> 99000.
        assert_eq!(stats.scratch[49], 50_000);
        assert_eq!(stats.scratch[94], 95_000);
        assert_eq!(stats.scratch[98], 99_000);
    }

    // Helper: monotonic ns after `n` intervals of n*1000 us each is the running
    // sum of 1000,2000,... but the test only needs distinct increasing deltas,
    // so use a fixed 1000 us * n step giving interval n = 1000*n us.
    fn cumulative_us(n: u64) -> u64 {
        // interval k (1-based) = 1000*k us; cumulative ns = sum_{k=1}^{n} 1000*k us.
        let sum_us: u64 = (1..=n).map(|k| 1_000 * k).sum();
        sum_us * 1_000
    }
}
