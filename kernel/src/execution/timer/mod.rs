use alloc::{boxed::Box, vec::Vec};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use spin::Once;

use crate::{arch, execution::cpu, platform};

mod deadline;

/// scheduler tick 频率（每秒 tick 数）；每个 CPU 以 timebase / HZ 的固定间隔编程 local timer。
const TICKS_PER_SEC: u64 = 100;
const _: () = assert!(TICKS_PER_SEC != 0);

// OWNER: timer module owns the calibrated scheduler tick interval.
static TICK_INTERVAL_VALUE: AtomicU64 = AtomicU64::new(0);

/// boot CPU 空闲时的 housekeeping 周期（微秒）：崩溃看门狗要在 deferred 推进停滞时仍能被调度到，
/// 周期远小于其 30 s 停滞上限。缺少它时全空闲系统不再有 timer 中断，看门狗与 deferred 进度都冻结。
const HOUSEKEEPING_US: u64 = 1_000_000;

/// 一个 logical CPU 的 timer 状态。
///
/// 硬件 deadline 由三个来源取最小值合成：运行线程时的调度 tick、本 CPU 最早的截止时间等待者、
/// boot CPU 的 housekeeping。除 `earliest_ns` 外，字段都只在本 CPU 且本地 IRQ 屏蔽时读写。
struct CpuTimerState {
    // OWNER: 调度 tick 的相位。若从 handler 完成时刻重算，延迟会累积并使 tick 漂移。
    tick_deadline: AtomicU64,
    // FLAG: 本 CPU 正在运行线程，需要调度 tick；空闲时清除以免无谓唤醒。
    tick_active: AtomicBool,
    // OWNER: 本 CPU 定时队列队首的截止时间（纳秒，`u64::MAX` 为空）。由队列锁内的发布者写入，
    // 远端取消只会让它变晚，因此本地读到旧值最多多一次空中断，不会丢失到期。
    earliest_ns: AtomicU64,
    // OWNER: boot CPU 的下一次 housekeeping 时刻；其他 CPU 不使用。
    housekeeping_deadline: AtomicU64,
    // OWNER: 最近一次写入硬件的 deadline，供中断确认时报告“本次到期的计划时刻”。
    programmed: AtomicU64,
    // FLAG: timer source 当前是否开放；没有任何截止来源时关闭，缺失会让空闲 CPU 被无效中断唤醒。
    source_enabled: AtomicBool,
    // FLAG: 本 CPU 已完成 `enable_timer_interrupt`；之前入队的等待者只记录队首，由首次编程统一生效。
    ready: AtomicBool,
    // OWNER: 仅由对应 logical CPU 在返回用户态前写入。内核态 IRQ 被屏蔽，到期的 tick 要到返回用户态才被
    // 响应；缺失这个时刻时所有 tick 都像发生在用户态，线程的内核态时间恒为零。
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

/// 设置墙钟时间：把 realtime 与 monotonic 之间的偏移改成使当前 realtime 等于 `realtime_ns`。
///
/// # Parameters
///
/// - `realtime_ns`: 新的 Unix epoch 纳秒值。
///
/// # Errors
///
/// 早于本次启动的 monotonic 时间（偏移会为负）返回 `Err(())`；偏移是无符号量，这样的值无法表示。
///
/// 内核定时器只使用 monotonic deadline，墙钟跳变不影响任何已安排的等待；需要随墙钟重新求值的
/// 绝对定时器由其所属的用户态服务在收到时钟变化后自行重排。
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

/// 单个 timebase tick 的纳秒数，向上取整且至少为 1ns。
pub(crate) fn monotonic_resolution_ns() -> u64 {
    (NSEC_PER_SEC as u128).div_ceil(u128::from(platform::timebase_frequency())) as u64
}

/// 调度器 tick 间隔（纳秒）：每个 tick 都可能触发抢占，即基础时间片。
pub(crate) fn scheduler_quantum_ns() -> u64 {
    NSEC_PER_SEC / TICKS_PER_SEC
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

/// 一次本地 timer 中断的确认结果。
pub(crate) struct TimerEvent {
    /// 本次到期的计划时刻（counter）；tick 取其相位上的计划时刻，用于区分用户/内核态时间。
    pub(crate) fired: u64,
    /// 是否包含调度 tick；否则只是截止时间等待者或 housekeeping 到期。
    pub(crate) tick: bool,
}

fn local_state() -> &'static CpuTimerState {
    &CPU_DEADLINES.wait()[cpu::current_id().index()]
}

/// 纳秒截止时间换算为 counter，向上取整：早于截止时间唤醒会让等待者提前返回。
fn counter_for_ns(deadline_ns: u64) -> u64 {
    let frequency = u128::from(platform::timebase_frequency());
    (u128::from(deadline_ns) * frequency)
        .div_ceil(u128::from(NSEC_PER_SEC))
        .min(u128::from(u64::MAX)) as u64
}

fn tick_interval() -> u64 {
    let interval = TICK_INTERVAL_VALUE.load(Ordering::Acquire);
    assert!(
        interval != 0,
        "timer interval used before per-CPU initialization"
    );
    interval
}

/// 按当前来源合成并写入本 CPU 的硬件 deadline；调用者必须屏蔽本地 IRQ。
///
/// 没有任何来源时关闭 timer source。先写 deadline 再开放 source：反序会让恢复中的 stale level
/// 先于新 CVAL 交付。
fn reprogram_local() {
    let state = local_state();
    if !state.ready.load(Ordering::Relaxed) {
        return;
    }
    let mut target = None::<u64>;
    let mut include = |deadline: u64| target = Some(target.map_or(deadline, |t| t.min(deadline)));
    if state.tick_active.load(Ordering::Relaxed) {
        include(state.tick_deadline.load(Ordering::Relaxed));
    }
    let earliest = state.earliest_ns.load(Ordering::Relaxed);
    if earliest != u64::MAX {
        include(counter_for_ns(earliest));
    }
    if cpu::current_id() == cpu::boot_id() {
        include(state.housekeeping_deadline.load(Ordering::Relaxed));
    }
    match target {
        Some(deadline) => {
            platform::arm_timer(deadline).expect("platform timer programming failed");
            state.programmed.store(deadline, Ordering::Relaxed);
            if !state.source_enabled.swap(true, Ordering::Relaxed) {
                // SAFETY: the deadline above is already programmed on this CPU, and the caller keeps
                // local IRQ delivery masked until it returns.
                unsafe { crate::arch::interrupt::enable_timer_source() };
            }
        }
        None => {
            if state.source_enabled.swap(false, Ordering::Relaxed) {
                arch::interrupt::disable_timer_source();
            }
        }
    }
}

/// 发布 `cpu` 定时队列的队首截止时间；调用者持有该队列锁。
///
/// 目标是当前 CPU 时同步重新编程硬件 deadline（新等待者可能早于已编程的 tick）；远端 CPU 只更新队首，
/// 其本地中断路径会在下一次确认时重新合成。
pub(crate) fn publish_timer_head(target: cpu::CpuId, head_ns: Option<u64>) {
    let state = &CPU_DEADLINES.wait()[target.index()];
    state
        .earliest_ns
        .store(head_ns.unwrap_or(u64::MAX), Ordering::Relaxed);
    if target == cpu::current_id() {
        reprogram_local();
    }
}

/// 确认本地 timer 中断：推进已到期的来源，重新编程下一个 deadline（同时让 level source 失效）。
///
/// 必须先于中断控制器 EOI 调用，反序会让 GIC 立即重投同一 PPI。
pub(crate) fn acknowledge_timer_interrupt() -> TimerEvent {
    let state = local_state();
    let now = arch::time::counter();
    let mut event = TimerEvent {
        fired: state.programmed.load(Ordering::Relaxed),
        tick: false,
    };
    if state.tick_active.load(Ordering::Relaxed) {
        let planned = state.tick_deadline.load(Ordering::Relaxed);
        if now >= planned {
            event = TimerEvent {
                fired: planned,
                tick: true,
            };
            let next = deadline::next(planned, now, tick_interval())
                .expect("timer deadline exhausted the time counter");
            state.tick_deadline.store(next, Ordering::Relaxed);
        }
    }
    if cpu::current_id() == cpu::boot_id()
        && now >= state.housekeeping_deadline.load(Ordering::Relaxed)
    {
        state.housekeeping_deadline.store(
            now.saturating_add(housekeeping_interval()),
            Ordering::Relaxed,
        );
    }
    reprogram_local();
    event
}

fn housekeeping_interval() -> u64 {
    (u128::from(platform::timebase_frequency()) * u128::from(HOUSEKEEPING_US)
        / u128::from(USEC_PER_SEC)) as u64
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
/// - `fired_deadline`: 该 tick 的计划到期时刻，即 [`TimerEvent::fired`]。
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

    // 2. Release 发布 interval，`tick_interval` 的 Acquire 保证不会读到未初始化值。
    TICK_INTERVAL_VALUE.store(interval, Ordering::Release);
    // 3. 先建立本 CPU 的 tick 相位与 housekeeping，再标记 ready 并首次编程；此前入队的等待者
    //    只记录了队首，由这次编程统一生效。
    let state = local_state();
    let now = arch::time::counter();
    state.tick_deadline.store(now + interval, Ordering::Relaxed);
    state.tick_active.store(true, Ordering::Relaxed);
    state
        .housekeeping_deadline
        .store(now + housekeeping_interval(), Ordering::Relaxed);
    state.ready.store(true, Ordering::Relaxed);
    reprogram_local();
}

/// 本 CPU 进入 scheduler idle 前停止调度 tick。
///
/// 之后的硬件 deadline 只来自本 CPU 的截止时间等待者（boot CPU 另有 housekeeping）；都没有时关闭
/// timer source，空闲 CPU 不再被周期中断唤醒。
///
/// # Errors
///
/// caller 必须持有当前 CPU 的 local IRQ guard，且当前 CPU 不得运行 task。
pub(crate) fn suspend_local_idle_tick() {
    local_state().tick_active.store(false, Ordering::Relaxed);
    reprogram_local();
}

/// 本 CPU 离开 scheduler idle、运行 task 前恢复调度 tick。
///
/// 保持原相位并跳过已错过的周期，首个 deadline 严格晚于当前 counter。
///
/// # Errors
///
/// caller 必须持有当前 CPU 的 local IRQ guard，且 timer 已完成全局初始化。
pub(crate) fn resume_local_idle_tick() {
    let state = local_state();
    let next = deadline::next(
        state.tick_deadline.load(Ordering::Relaxed),
        arch::time::counter(),
        tick_interval(),
    )
    .expect("timer deadline exhausted the time counter");
    state.tick_deadline.store(next, Ordering::Relaxed);
    state.tick_active.store(true, Ordering::Relaxed);
    reprogram_local();
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
        tick_deadline: AtomicU64::new(0),
        tick_active: AtomicBool::new(false),
        earliest_ns: AtomicU64::new(u64::MAX),
        housekeeping_deadline: AtomicU64::new(0),
        programmed: AtomicU64::new(0),
        source_enabled: AtomicBool::new(false),
        ready: AtomicBool::new(false),
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
