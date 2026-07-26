#[derive(Clone, Copy)]
pub(crate) struct MixerMetrics {
    pub(crate) xrun_count: u64,
    pub(crate) period_count: u64,
    pub(crate) mix_p99_us: u64,
    pub(crate) limiter_activations: u64,
    pub(crate) limiter_max_reduction: f32,
    pub(crate) steady_allocations: u64,
}

pub(super) struct TimingHistogram {
    // 50 us buckets through 6.35 ms; the final bucket includes larger samples.
    buckets: [u64; 128],
    samples: u64,
}

impl TimingHistogram {
    pub(super) const fn new() -> Self {
        Self {
            buckets: [0; 128],
            samples: 0,
        }
    }

    pub(super) fn record(&mut self, elapsed_us: u64) {
        let bucket = (elapsed_us / 50).min((self.buckets.len() - 1) as u64) as usize;
        self.buckets[bucket] = self.buckets[bucket].saturating_add(1);
        self.samples = self.samples.saturating_add(1);
    }

    pub(super) fn percentile_99_us(&self) -> u64 {
        if self.samples == 0 {
            return 0;
        }
        let target = self.samples.saturating_mul(99).div_ceil(100);
        let mut total = 0;
        for (bucket, count) in self.buckets.iter().enumerate() {
            total += count;
            if total >= target {
                return (bucket as u64 + 1) * 50;
            }
        }
        self.buckets.len() as u64 * 50
    }
}
