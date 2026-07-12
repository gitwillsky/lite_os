use alloc::{collections::BTreeSet, vec::Vec};

use crate::{
    arch::hart,
    task::processor::request_reschedule,
    timer::{get_time_ns, get_time_us},
};

use super::{
    ProcessState, TASK_MANAGER, process_terminal_input, procfs, send_process_signal,
    wake_console_waiters, wake_expired_tasks,
};

#[derive(Clone, Copy)]
pub(super) struct RealTimer {
    next_expiration_us: Option<u64>,
    interval_us: u64,
}

fn expire_real_timers(now_us: u64) {
    let targets = {
        let mut graph = TASK_MANAGER.graph.lock();
        let mut targets = Vec::new();
        let live: BTreeSet<_> = graph
            .nodes
            .iter()
            .filter_map(|(&tgid, node)| matches!(node.state, ProcessState::Live(_)).then_some(tgid))
            .collect();
        for (&tgid, timer) in &mut graph.real_timers {
            let Some(expiration) = timer.next_expiration_us else {
                continue;
            };
            if expiration > now_us || !live.contains(&tgid) {
                continue;
            }
            targets.push(tgid);
            timer.next_expiration_us = now_us
                .saturating_sub(expiration)
                .checked_div(timer.interval_us)
                .and_then(|elapsed_periods| {
                    expiration.checked_add(
                        elapsed_periods
                            .saturating_add(1)
                            .saturating_mul(timer.interval_us),
                    )
                });
        }
        targets
    };
    for tgid in targets {
        let _ = send_process_signal(tgid as i32, 14);
    }
}

/// @description 原子替换 Process 的 ITIMER_REAL，并返回旧 timer 的剩余时间与 interval。
pub(crate) fn set_real_timer(
    tgid: usize,
    value_us: u64,
    interval_us: u64,
    now_us: u64,
) -> Result<(u64, u64), ()> {
    let mut graph = TASK_MANAGER.graph.lock();
    if !graph
        .nodes
        .get(&tgid)
        .is_some_and(|node| matches!(node.state, ProcessState::Live(_)))
    {
        return Err(());
    }
    let previous = graph.real_timers.insert(
        tgid,
        RealTimer {
            next_expiration_us: (value_us != 0).then(|| now_us.saturating_add(value_us)),
            interval_us,
        },
    );
    Ok(previous.map_or((0, 0), |timer| {
        (
            timer
                .next_expiration_us
                .map_or(0, |expiration| expiration.saturating_sub(now_us)),
            timer.interval_us,
        )
    }))
}

pub(crate) fn real_timer(tgid: usize, now_us: u64) -> Result<(u64, u64), ()> {
    let graph = TASK_MANAGER.graph.lock();
    if !graph
        .nodes
        .get(&tgid)
        .is_some_and(|node| matches!(node.state, ProcessState::Live(_)))
    {
        return Err(());
    }
    Ok(graph.real_timers.get(&tgid).map_or((0, 0), |timer| {
        (
            timer
                .next_expiration_us
                .map_or(0, |expiration| expiration.saturating_sub(now_us)),
            timer.interval_us,
        )
    }))
}

/// @description 在 user-return 或 scheduler idle context 消费全部 deferred work。
pub(crate) fn dispatch_pending_deferred_work() {
    let work = hart::take_softirqs();
    if work == 0 {
        return;
    }
    if work & hart::TIMER_SOFTIRQ != 0 {
        wake_expired_tasks(get_time_ns());
        procfs::update_load_average(get_time_us());
        expire_real_timers(get_time_us());
    }
    if work & hart::CONSOLE_SOFTIRQ != 0 {
        process_terminal_input();
        wake_console_waiters();
    }
    let network_due = work & hart::NETWORK_SOFTIRQ != 0
        || work & hart::TIMER_SOFTIRQ != 0 && crate::socket::network_work_due();
    if network_due {
        // RX budget 用尽时必须再次发布同一 deferred work；否则 used ring 中没有新 IRQ edge
        // 的 frame 可能永久滞留。timer deadline 同样在此推进 ARP/UDP egress；缺失时丢失
        // 首个 ARP reply 后将永远不重试。requeue 由 task deferred owner 执行，socket 不反向依赖 arch。
        if crate::socket::dispatch_network_work() {
            hart::raise_network_softirq();
        }
    }
    request_reschedule();
}
