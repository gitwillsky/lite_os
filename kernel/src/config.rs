// QEMU 默认时钟频率
pub const CLOCK_FREQ: usize = 10_000_000;

// 时钟中断的频率
pub const TICKS_PER_SEC: usize = 100;

// 时钟中断的间隔时间
pub const TICK_INTERVAL: usize = CLOCK_FREQ / TICKS_PER_SEC;
