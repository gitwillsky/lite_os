use alloc::{boxed::Box, vec::Vec};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use spin::Once;

use crate::{arch, execution::cpu, platform};

/// 基础时间片（纳秒）：同一 CPU 上有竞争者时，当前线程连续运行的上限。
const SCHEDULER_QUANTUM_NS: u64 = 10_000_000;

/// boot CPU 空闲时的 housekeeping 周期（微秒）：崩溃看门狗要在 deferred 推进停滞时仍能被调度到，
/// 周期远小于其 30 s 停滞上限。缺少它时全空闲系统不再有 timer 中断，看门狗与 deferred 进度都冻结。
const HOUSEKEEPING_US: u64 = 1_000_000;

/// 一个 logical CPU 的 timer 状态。
///
/// 硬件 deadline 由三个来源取最小值合成：调度截止（公平片、CBS 预算耗尽/补充）、
/// 本 CPU 最早的截止时间等待者、boot CPU 的 housekeeping。除 `earliest_ns` 外，字段都只在本 CPU 且本地
/// IRQ 屏蔽时读写。没有来源的 CPU 关闭 timer source：忙碌但无竞争的 CPU 也没有周期中断。
struct CpuTimerState {
    // OWNER: 调度器最早事件（counter，MAX 为无）；公平片只在有竞争者时武装，预算补充在 idle 也保留。
    // 缺少它，公平竞争者无法抢占，或耗尽预算者永远不能补充/无竞争者可无限超额。
    slice_deadline: AtomicU64,
    // OWNER: 当前时间片开始的时刻（counter）；竞争者在时间片中途到达时据此决定截止时刻。
    slice_start: AtomicU64,
    // OWNER: 定时队列发表、尚未由 timer IRQ 领取的队首截止（纳秒，MAX 为无待编程来源）。
    // IRQ 领取后清空，deferred expire 持队列锁重新发表真实队首；队列本身仍唯一拥有等待者。
    // 若在 EOI 前继续编程已经到期的队首，level timer 会无限重投，deferred 永远没有机会推进。
    earliest_ns: AtomicU64,
    // OWNER: boot CPU 的下一次 housekeeping 时刻；其他 CPU 不使用。
    housekeeping_deadline: AtomicU64,
    // OWNER: 最近一次写入硬件的 deadline。
    programmed: AtomicU64,
    // FLAG: timer source 当前是否开放；没有任何截止来源时关闭，缺失会让空闲 CPU 被无效中断唤醒。
    source_enabled: AtomicBool,
    // FLAG: 本 CPU 已完成 `enable_timer_interrupt`；之前入队的等待者只记录队首，由首次编程统一生效。
    ready: AtomicBool,
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

/// 基础时间片（纳秒）：有竞争者时，当前线程在被抢占前最多连续运行这么久。
pub(crate) fn scheduler_quantum_ns() -> u64 {
    SCHEDULER_QUANTUM_NS
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
    /// 是否包含调度截止（公平片、预算耗尽或补充）；否则只是等待者或 housekeeping 到期。
    pub(crate) slice_expired: bool,
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

fn quantum_counter() -> u64 {
    counter_for_ns(SCHEDULER_QUANTUM_NS)
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
    let slice = state.slice_deadline.load(Ordering::Relaxed);
    if slice != u64::MAX {
        include(slice);
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
        slice_expired: false,
    };
    if now >= state.slice_deadline.load(Ordering::Relaxed) {
        // 一次性来源：safe point 重新选择/补充，并按当前预算或竞争状态重装。
        state.slice_deadline.store(u64::MAX, Ordering::Relaxed);
        event.slice_expired = true;
    }
    if cpu::current_id() == cpu::boot_id()
        && now >= state.housekeeping_deadline.load(Ordering::Relaxed)
    {
        state.housekeeping_deadline.store(
            now.saturating_add(housekeeping_interval()),
            Ordering::Relaxed,
        );
    }
    // 1. 本次 timer IRQ 总会发布 TIMER/TIMER_BACKLOG，deferred expire 随后重发实际队首。
    // 2. 先领取这份编程来源再 EOI；即使远端取消刚发表更晚的队首，expire 也会在队列锁内恢复它。
    // 3. 不能沿用过期 CVAL，否则 eret 立即再次入 IRQ，形成抢在 deferred 之前的 level 中断风暴。
    state.earliest_ns.store(u64::MAX, Ordering::Relaxed);
    reprogram_local();
    event
}

/// 记录当前线程的时间片从现在开始；切换入线程时调用（调用者屏蔽本地 IRQ）。
pub(crate) fn note_slice_start() {
    local_state()
        .slice_start
        .store(arch::time::counter(), Ordering::Relaxed);
}

/// 调度器唯一 deadline 来源：公平时间片、CBS 预算耗尽与最早预算补充取最小。
/// 调用者屏蔽本地 IRQ；即使 idle 或无竞争者也保留预算补充，不能沿用旧线程的截止。
pub(crate) fn set_scheduler_deadline(fair_competes: bool, scheduler_ns: Option<u64>) {
    let state = local_state();
    let fair = if fair_competes {
        state
            .slice_start
            .load(Ordering::Relaxed)
            .saturating_add(quantum_counter())
    } else {
        u64::MAX
    };
    let deadline = fair.min(scheduler_ns.map_or(u64::MAX, counter_for_ns));
    if state.slice_deadline.swap(deadline, Ordering::Relaxed) != deadline {
        reprogram_local();
    }
}

fn housekeeping_interval() -> u64 {
    (u128::from(platform::timebase_frequency()) * u128::from(HOUSEKEEPING_US)
        / u128::from(USEC_PER_SEC)) as u64
}

pub(crate) fn enable_timer_interrupt() {
    // DTB 的 timebase-frequency 是平台契约，零值不能被静默改写为伪造频率。
    assert!(
        platform::timebase_frequency() != 0,
        "DTB timebase-frequency must be non-zero"
    );
    // 先建立 housekeeping，再标记 ready 并首次编程；此前入队的等待者只记录了队首，
    // 由这次编程统一生效。
    let state = local_state();
    let now = arch::time::counter();
    state
        .housekeeping_deadline
        .store(now + housekeeping_interval(), Ordering::Relaxed);
    state.ready.store(true, Ordering::Relaxed);
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
        slice_deadline: AtomicU64::new(u64::MAX),
        slice_start: AtomicU64::new(0),
        earliest_ns: AtomicU64::new(u64::MAX),
        housekeeping_deadline: AtomicU64::new(0),
        programmed: AtomicU64::new(0),
        source_enabled: AtomicBool::new(false),
        ready: AtomicBool::new(false),
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
