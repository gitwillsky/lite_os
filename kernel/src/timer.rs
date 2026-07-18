use alloc::{boxed::Box, vec::Vec};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use spin::Once;

use crate::{arch, config, cpu, platform};

mod deadline;

// OWNER: timer module owns the calibrated scheduler tick interval.
static TICK_INTERVAL_VALUE: AtomicU64 = AtomicU64::new(0);

// OWNER: timer module 的每个 slot 仅由对应 logical CPU 推进；Atomic 只为静态共享 table
// 提供 interior mutability。若从 handler 完成时刻重算，延迟会累积并使 scheduler tick 漂移。
static CPU_DEADLINES: Once<Box<[AtomicU64]>> = Once::new();

const USEC_PER_SEC: u64 = 1_000_000;
const NSEC_PER_SEC: u64 = 1_000_000_000;

// 系统启动时的时间偏移，从 platform realtime source 获取真实时间。
// OWNER: timer module owns the boot-time offset from monotonic to realtime clock.
static REALTIME_OFFSET_NS: AtomicU64 = AtomicU64::new(0);
// OWNER: timer module publishes whether the realtime offset is valid.
static REALTIME_INITIALIZED: AtomicBool = AtomicBool::new(false);

/// @description 返回 Unix epoch realtime 纳秒值。
///
/// @return RTC 启动 offset 加 monotonic；初始化前直接读取 platform realtime，失败则使用固定 epoch offset。
pub(crate) fn get_realtime_ns() -> u64 {
    if REALTIME_INITIALIZED.load(Ordering::Acquire) {
        return REALTIME_OFFSET_NS
            .load(Ordering::Relaxed)
            .saturating_add(get_time_ns());
    }
    platform::read_realtime_ns().unwrap_or(1_704_067_200u64 * NSEC_PER_SEC)
}

/// @description 返回本次启动时刻对应的 Unix epoch 秒数。
///
/// @return RTC 校准得到的 realtime offset，按秒向下取整。
/// @panics `init_rtc` 尚未发布 realtime offset 时 panic，避免 procfs 输出伪造启动时间。
pub(crate) fn boot_epoch_seconds() -> u64 {
    assert!(
        REALTIME_INITIALIZED.load(Ordering::Acquire),
        "boot epoch read before realtime initialization"
    );
    REALTIME_OFFSET_NS.load(Ordering::Relaxed) / NSEC_PER_SEC
}

/// @description 将 absolute realtime timestamp 转换为同一启动域的 monotonic deadline。
///
/// @param realtime_ns Unix epoch 纳秒 timestamp。
/// @return 减去 immutable boot offset 的 monotonic deadline；已早于 monotonic epoch 时返回零。
/// @panics `init_rtc` 尚未发布 realtime offset 时 panic，避免用未校准时钟安排 sleep。
pub(crate) fn realtime_deadline_to_monotonic_ns(realtime_ns: u64) -> u64 {
    assert!(
        REALTIME_INITIALIZED.load(Ordering::Acquire),
        "realtime deadline converted before RTC initialization"
    );
    realtime_ns.saturating_sub(REALTIME_OFFSET_NS.load(Ordering::Relaxed))
}

pub(crate) fn get_time_us() -> u64 {
    let current_mtime = arch::time::counter();
    let time_base_freq = platform::timebase_frequency();
    // 使用128位运算避免溢出
    ((current_mtime as u128 * USEC_PER_SEC as u128) / time_base_freq as u128) as u64
}

pub(crate) fn get_time_ns() -> u64 {
    let current_mtime = arch::time::counter();
    let time_base_freq = platform::timebase_frequency();
    // 使用128位运算避免溢出
    ((current_mtime as u128 * NSEC_PER_SEC as u128) / time_base_freq as u128) as u64
}

/// @description 返回 DTB time counter 经整数纳秒换算后的最小可观察粒度。
///
/// @return 单个 timebase tick 的纳秒数，向上取整且至少为 1ns。
/// @panics DTB `timebase-frequency` 为零时 panic；该平台契约缺失时不能伪造分辨率。
pub(crate) fn monotonic_resolution_ns() -> u64 {
    let frequency = platform::timebase_frequency();
    assert!(frequency != 0, "DTB timebase-frequency must be non-zero");
    (NSEC_PER_SEC as u128).div_ceil(frequency as u128) as u64
}

/// @description 返回 timer owner 实际用于 scheduler preemption 的基础时间片。
///
/// @return 已校准 tick interval 对应的纳秒数，向上取整。
/// @errors timer 尚未初始化或 DTB timebase-frequency 为零时 fail-stop。
pub(crate) fn scheduler_quantum_ns() -> u64 {
    let interval = TICK_INTERVAL_VALUE.load(Ordering::Acquire);
    assert_ne!(
        interval, 0,
        "scheduler quantum read before timer initialization"
    );
    let frequency = platform::timebase_frequency();
    assert_ne!(frequency, 0, "DTB timebase-frequency must be non-zero");
    (interval as u128 * NSEC_PER_SEC as u128).div_ceil(frequency as u128) as u64
}

#[inline(always)]
pub(crate) fn set_next_timer_interrupt() {
    let current_mtime = arch::time::counter();
    let interval = TICK_INTERVAL_VALUE.load(Ordering::Acquire);
    assert!(
        interval != 0,
        "timer interval used before per-CPU initialization"
    );
    let state = &CPU_DEADLINES.wait()[cpu::current_id().index()];
    let previous = state.load(Ordering::Relaxed);
    let next_mtime = deadline::next(previous, current_mtime, interval)
        .expect("timer deadline exhausted the time counter");
    state.store(next_mtime, Ordering::Relaxed);

    platform::arm_timer(next_mtime).expect("platform timer programming failed");
}

pub(crate) fn enable_timer_interrupt() {
    let time_base_freq = platform::timebase_frequency();

    // 1. DTB 的 timebase-frequency 是平台契约，零值不能被静默改写为伪造频率。
    assert!(
        time_base_freq != 0,
        "DTB timebase-frequency must be non-zero"
    );
    let ticks_per_sec = config::TICKS_PER_SEC as u64;
    assert!(ticks_per_sec != 0, "TICKS_PER_SEC must be non-zero");
    let interval = time_base_freq / ticks_per_sec;
    assert!(interval != 0, "timer tick rate exceeds timebase frequency");

    // 2. Release 发布 interval，set_next_timer_interrupt 的 Acquire 保证不会读到未初始化值。
    TICK_INTERVAL_VALUE.store(interval, Ordering::Release);
    // SAFETY: timer initialization changes only the current CPU's architecture timer source.
    unsafe {
        // 3. 每个 CPU 独立启用 timer source，并在打开 scheduler interrupts 前写入首个 deadline。
        crate::arch::interrupt::enable_timer_source();
    }

    set_next_timer_interrupt();
}

pub(crate) fn init_rtc() {
    assert!(
        CPU_DEADLINES.get().is_none(),
        "timer CPU state initialized twice"
    );
    let mut deadlines = Vec::new();
    deadlines
        .try_reserve_exact(cpu::count())
        .expect("timer CPU state allocation failed");
    deadlines.extend((0..cpu::count()).map(|_| AtomicU64::new(0)));
    CPU_DEADLINES.call_once(|| deadlines.into_boxed_slice());

    if let Some(current_unix_ns) = platform::read_realtime_ns() {
        let offset = current_unix_ns.saturating_sub(get_time_ns());
        REALTIME_OFFSET_NS.store(offset, Ordering::Relaxed);
        REALTIME_INITIALIZED.store(true, Ordering::Release);
        debug!("Realtime offset set to {} ns (from platform)", offset);
    } else {
        warn!("Platform realtime source unavailable, using default boot time");
        REALTIME_OFFSET_NS.store(1_704_067_200u64 * NSEC_PER_SEC, Ordering::Relaxed);
        REALTIME_INITIALIZED.store(true, Ordering::Release);
    }
    debug!("timer initialized with real-time clock");
}
