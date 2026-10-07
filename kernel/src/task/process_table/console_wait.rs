use super::*;

/// deferred 阶段把 UART raw input 推进系统 console 的 line discipline。
///
/// # Returns
///
/// raw input 仍有 backlog 时为 true。
pub(super) fn process_terminal_input() -> bool {
    match crate::tty::drain_input(&crate::tty::console()) {
        Ok(backlog) => backlog,
        Err(_) => {
            debug!("TTY line discipline failed to drain UART input");
            false
        }
    }
}

/// 在统一 wait registry 中阻塞当前 console reader，封闭 read/enqueue IRQ race。
///
/// # Parameters
///
/// - `deadline`: VTIME 导出的 absolute monotonic deadline；无超时时为 None。
/// - `input_ready`: registration publication 后、registry shard lock 外复查 UART ring。
///
/// # Returns
///
/// 输入已到达/IRQ 唤醒返回 `Woken`，到期返回 `TimedOut`，signal cancellation 返回 `Interrupted`。
pub(super) fn wait_for_console(
    deadline: Option<u64>,
    input_ready: &dyn Fn() -> bool,
) -> WaitResult {
    let task = current_task().expect("console wait requires current task");
    let ticket = WAIT_REGISTRY.allocate_ticket();
    let prepared = ticket.prepare_console(deadline, task.clone());
    arm_indexed_wait(
        &task,
        prepared,
        || {
            if input_ready() {
                Some(WaitResult::Woken)
            } else if deadline.is_some_and(|value| value <= get_time_ns()) {
                Some(WaitResult::TimedOut)
            } else if task.has_deliverable_signal() {
                Some(WaitResult::Interrupted)
            } else {
                None
            }
        },
        WaitMembership::Console,
    )
    .map_or_else(core::convert::identity, |prepared| prepared.suspend())
}

pub(super) fn wake_console_waiters() -> bool {
    const INPUT: i16 = 0x001;
    crate::file::Epoll::notify_console_source();
    let mut batch = console_batch::ConsoleWakeBatch::new();
    while !batch.is_full() {
        let Some(wake) = WAIT_REGISTRY.wake_console_one(false, INPUT, batch.groups()) else {
            break;
        };
        batch.record(wake.group);
        wake_claimed_console(wake.claimed);
    }
    if batch.is_full() {
        return true;
    }
    if let Some(wake) = WAIT_REGISTRY.wake_console_one(true, INPUT, batch.groups()) {
        batch.record(wake.group);
        wake_claimed_console(wake.claimed);
    }
    false
}

fn wake_claimed_console(claimed: Option<wait_registry::ClaimedWait>) {
    if let Some(claimed) = claimed {
        match claimed.kind {
            IndexedWaitKind::Console => {
                let _ = crate::task::processor::wake_console_task(
                    claimed.task,
                    claimed.id,
                    WaitResult::Woken,
                );
            }
            IndexedWaitKind::Poll => {
                let _ = crate::task::processor::wake_poll_task(
                    claimed.task,
                    claimed.id,
                    WaitResult::Woken,
                );
            }
            _ => panic!("console index contains non-console wait"),
        }
    }
}
