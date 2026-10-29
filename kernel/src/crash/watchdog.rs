//! 软看门狗：boot CPU 的硬中断 tick 仍在到达，但该 CPU 的 deferred work 已长时间没有推进。
//!
//! deferred TIMER work 在 user-return / idle safe point 消费。tick 继续到达而这里一直不被消费，说明
//! 该 CPU 卡在 safe point 之前的某个等待里（典型是 deferred 路径等待一把永远不释放的锁），
//! 用户态已经冻结。局限：所有 CPU 都在 IRQ 屏蔽状态下死锁时 tick 本身消失，纯软件无法发现，
//! 那需要 pseudo-NMI 或硬件 watchdog。

use core::sync::atomic::{AtomicU64, Ordering};

use crate::{execution::cpu, execution::timer};

/// deferred work 停滞超过该时长即视为 hang。
const STALL_LIMIT_US: u64 = 30_000_000;

// OWNER: 本模块独占 boot CPU 最近一次消费 deferred TIMER work 的时刻（微秒，0 = 尚未开始）。
// 缺少它，boot CPU 上的 deferred 死锁只会表现为系统静默冻结，没有任何现场。
static BOOT_CPU_DEFERRED_US: AtomicU64 = AtomicU64::new(0);

/// boot CPU 完成一次 deferred TIMER work 消费时调用。
pub(crate) fn note_deferred_progress() {
    if cpu::current_id() == cpu::boot_id() {
        BOOT_CPU_DEFERRED_US.store(timer::get_time_us(), Ordering::Relaxed);
    }
}

/// 在 boot CPU 的硬中断 tick 中检查停滞。
///
/// # Panics
///
/// deferred work 停滞超过 [`STALL_LIMIT_US`] 时 panic，由崩溃流程保存现场并显示蓝屏。
pub(crate) fn check_tick() {
    if cpu::current_id() != cpu::boot_id() {
        return;
    }
    let progress = BOOT_CPU_DEFERRED_US.load(Ordering::Relaxed);
    if progress == 0 {
        // 首次 tick：scheduler 尚未开始消费，以此刻为起点。
        BOOT_CPU_DEFERRED_US.store(timer::get_time_us().max(1), Ordering::Relaxed);
        return;
    }
    let stalled = timer::get_time_us().saturating_sub(progress);
    if stalled > STALL_LIMIT_US {
        panic!(
            "watchdog: boot CPU deferred work made no progress for {} s",
            stalled / 1_000_000
        );
    }
}
