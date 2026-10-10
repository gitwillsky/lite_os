//! 所有 host hot-path benchmark 共用的既有次数、采样与拒绝阈值。
pub(crate) const ITERATIONS: u64 = 2_000_000;
pub(crate) const SAMPLES: usize = 5;
pub(crate) const MAX_NANOSECONDS_PER_OPERATION: f64 = 200.0;
