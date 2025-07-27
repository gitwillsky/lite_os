use core::sync::atomic::AtomicU64;

// 时钟中断的频率
pub const TICKS_PER_SEC: usize = 100;
pub static TICK_INTERVAL_VALUE: AtomicU64 = AtomicU64::new(0);
pub static TIMER_FREQ: AtomicU64 = AtomicU64::new(0);