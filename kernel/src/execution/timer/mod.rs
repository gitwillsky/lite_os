use alloc::{boxed::Box, vec::Vec};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use spin::Once;

use crate::{arch, execution::cpu, platform};

mod deadline;

/// scheduler tick 频率（Linux `HZ`）；每个 CPU 以 timebase / HZ 的固定间隔编程 local timer。
const TICKS_PER_SEC: u64 = 100;
/// Socket timeval 与实际 scheduler cadence 共用的 tick 时长；禁止在协议层复制 HZ。
pub(crate) const SCHEDULER_TICK_NS: u64 = 1_000_000_000 / TICKS_PER_SEC;
const _: () = assert!(TICKS_PER_SEC != 0);

// OWNER: timer module owns the calibrated scheduler tick interval.
static TICK_INTERVAL_VALUE: AtomicU64 = AtomicU64::new(0);

/// 一个 logical CPU 的 timer 状态。
struct CpuTimerState {
    // OWNER: 仅由对应 logical CPU 推进；若从 handler 完成时刻重算，延迟会累积并使 scheduler tick 漂移。
    deadline: AtomicU64,
    // OWNER: 仅由对应 logical CPU 在返回用户态前写入。内核态 IRQ 被屏蔽，到期的 tick 要到返回用户态才被
    // 响应；缺失这个时刻时所有 tick 都像发生在用户态，`getrusage` 的 system 时间恒为零。
    last_user_return: AtomicU64,
}

// OWNER: timer module 的每个 slot 仅由对应 logical CPU 访问；Atomic 只为静态共享 table 提供 interior mutability。
static CPU_DEADLINES: Once<Box<[CpuTimerState]>> = Once::new();

const USEC_PER_SEC: u64 = 1_000_000;
const NSEC_PER_SEC: u64 = 1_000_000_000;

// 系统启动时的时间偏移，从 platform realtime source 获取真实时间。
// OWNER: timer module owns the boot-time offset from monotonic to realtime clock.
static REALTIME_OFFSET_NS: AtomicU64 = AtomicU64::new(0);
// OWNER: timer module publishes whether the realtime offset is valid.
static REALTIME_INITIALIZED: AtomicBool = AtomicBool::new(false);

/// 返回 Unix epoch realtime 纳秒值。
///
/// # Returns
///
/// RTC 启动 offset 加 monotonic；初始化前直接读取 platform realtime，失败则使用固定 epoch offset。
pub(crate) fn get_realtime_ns() -> u64 {
    if REALTIME_INITIALIZED.load(Ordering::Acquire) {
        return REALTIME_OFFSET_NS
            .load(Ordering::Relaxed)
            .saturating_add(get_time_ns());
    }
    platform::read_realtime_ns().unwrap_or(1_704_067_200u64 * NSEC_PER_SEC)
}

/// 设置 `CLOCK_REALTIME`：把 realtime 与 monotonic 之间的偏移改成使当前 realtime 等于 `realtime_ns`。
///
/// # Parameters
///
/// - `realtime_ns`: 新的 Unix epoch 纳秒值。
///
/// # Errors
///
/// 早于本次启动的 monotonic 时间（偏移会为负）返回 `Err(())`；偏移是无符号量，这样的值无法表示。
///
/// 已经安排的绝对 `CLOCK_REALTIME` 定时器在创建时已换算成 monotonic deadline，不随这次跳变移动
/// （Linux 会重新求值）；该限制记录在 `docs/syscall-support/signal-time.md`。
pub(crate) fn set_realtime_ns(realtime_ns: u64) -> Result<(), ()> {
    assert!(
        REALTIME_INITIALIZED.load(Ordering::Acquire),
        "realtime clock set before RTC initialization"
    );
    let offset = realtime_ns.checked_sub(get_time_ns()).ok_or(())?;
    REALTIME_OFFSET_NS.store(offset, Ordering::Relaxed);
    Ok(())
}

/// 返回本次启动时刻对应的 Unix epoch 秒数。
///
/// # Returns
///
/// RTC 校准得到的 realtime offset，按秒向下取整。
///
/// # Panics
///
/// `init_rtc` 尚未发布 realtime offset 时 panic，避免 procfs 输出伪造启动时间。
pub(crate) fn boot_epoch_seconds() -> u64 {
    assert!(
        REALTIME_INITIALIZED.load(Ordering::Acquire),
        "boot epoch read before realtime initialization"
    );
    REALTIME_OFFSET_NS.load(Ordering::Relaxed) / NSEC_PER_SEC
}

/// 将 absolute realtime timestamp 转换为同一启动域的 monotonic deadline。
///
/// # Parameters
///
/// - `realtime_ns`: Unix epoch 纳秒 timestamp。
///
/// # Returns
///
/// 减去 immutable boot offset 的 monotonic deadline；已早于 monotonic epoch 时返回零。
///
/// # Panics
///
/// `init_rtc` 尚未发布 realtime offset 时 panic，避免用未校准时钟安排 sleep。
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

/// 返回 DTB time counter 经整数纳秒换算后的最小可观察粒度。
///
/// # Returns
///
/// 单个 timebase tick 的纳秒数，向上取整且至少为 1ns。
///
/// # Panics
///
/// DTB `timebase-frequency` 为零时 panic；该平台契约缺失时不能伪造分辨率。
pub(crate) fn monotonic_resolution_ns() -> u64 {
    let frequency = platform::timebase_frequency();
    assert!(frequency != 0, "DTB timebase-frequency must be non-zero");
    (NSEC_PER_SEC as u128).div_ceil(frequency as u128) as u64
}

/// 返回 timer owner 实际用于 scheduler preemption 的基础时间片。
///
/// # Returns
///
/// 已校准 tick interval 对应的纳秒数，向上取整。
///
/// # Errors
///
/// timer 尚未初始化或 DTB timebase-frequency 为零时 fail-stop。
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
/// 重新编程下一次 tick，返回刚刚到期的那次 tick 的计划时刻。
pub(crate) fn set_next_timer_interrupt() -> u64 {
    let current_mtime = arch::time::counter();
    let interval = TICK_INTERVAL_VALUE.load(Ordering::Acquire);
    assert!(
        interval != 0,
        "timer interval used before per-CPU initialization"
    );
    let state = &CPU_DEADLINES.wait()[cpu::current_id().index()].deadline;
    let previous = state.load(Ordering::Relaxed);
    let next_mtime = deadline::next(previous, current_mtime, interval)
        .expect("timer deadline exhausted the time counter");
    state.store(next_mtime, Ordering::Relaxed);

    platform::arm_timer(next_mtime).expect("platform timer programming failed");
    previous
}

/// 记录 calling CPU 即将返回用户态；必须是 trap 返回前的最后一步之一。
#[inline(always)]
pub(crate) fn note_return_to_user() {
    CPU_DEADLINES.wait()[cpu::current_id().index()]
        .last_user_return
        .store(arch::time::counter(), Ordering::Relaxed);
}

/// 判断一次在用户态 trap 里响应的 tick 是否真的发生在用户态。
///
/// # Parameters
///
/// - `fired_deadline`: 该 tick 的计划到期时刻，即 [`set_next_timer_interrupt`] 的返回值。
///
/// # Returns
///
/// 计划时刻晚于上次返回用户态的时刻为 true（tick 打断的是用户态）；否则它在内核态期间到期，
/// 只是被屏蔽到返回用户态才响应，应计为 system 时间。
pub(crate) fn tick_interrupted_user(fired_deadline: u64) -> bool {
    fired_deadline
        > CPU_DEADLINES.wait()[cpu::current_id().index()]
            .last_user_return
            .load(Ordering::Relaxed)
}

pub(crate) fn enable_timer_interrupt() {
    let time_base_freq = platform::timebase_frequency();

    // 1. DTB 的 timebase-frequency 是平台契约，零值不能被静默改写为伪造频率。
    assert!(
        time_base_freq != 0,
        "DTB timebase-frequency must be non-zero"
    );
    let interval = time_base_freq / TICKS_PER_SEC;
    assert!(interval != 0, "timer tick rate exceeds timebase frequency");

    // 2. Release 发布 interval，set_next_timer_interrupt 的 Acquire 保证不会读到未初始化值。
    TICK_INTERVAL_VALUE.store(interval, Ordering::Release);
    // 3. 先写首个 deadline，再开放 source；缺失此顺序会让恢复中的 stale level 先于新 CVAL 交付。
    set_next_timer_interrupt();
    // SAFETY: timer initialization changes only the current CPU's architecture timer source.
    unsafe {
        crate::arch::interrupt::enable_timer_source();
    }
}

/// 在非 boot CPU 进入 scheduler idle 前关闭本地周期 tick。
///
/// # Returns
///
/// 无返回值；boot CPU 不得调用，它保留 always-armed housekeeping/liveness tick。
///
/// # Errors
///
/// caller 必须持有当前 CPU 的 local IRQ guard，且当前 CPU 不得运行 task。
pub(crate) fn suspend_local_idle_tick() {
    assert_ne!(
        cpu::current_id(),
        cpu::boot_id(),
        "boot CPU keeps the always-armed scheduler tick"
    );
    arch::interrupt::disable_timer_source();
}

/// 在非 boot CPU 离开 scheduler idle、运行 task 前恢复本地抢占 tick。
///
/// # Returns
///
/// 无返回值；首个 deadline 严格晚于当前 counter。
///
/// # Errors
///
/// caller 必须持有当前 CPU 的 local IRQ guard，且 timer 已完成全局初始化。
pub(crate) fn resume_local_idle_tick() {
    set_next_timer_interrupt();
    // SAFETY: the fresh local deadline was programmed while local IRQ delivery remains masked.
    unsafe { arch::interrupt::enable_timer_source() };
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
    deadlines.extend((0..cpu::count()).map(|_| CpuTimerState {
        deadline: AtomicU64::new(0),
        last_user_return: AtomicU64::new(0),
    }));
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
