use super::*;

pub(super) fn process_terminal_input() -> bool {
    let terminal = {
        let graph = TASK_MANAGER.graph.lock();
        graph.nodes.values().find_map(|node| {
            let ProcessState::Live(threads) = &node.state else {
                return None;
            };
            threads.values().next().map(|task| task.terminal())
        })
    };
    let Some(terminal) = terminal else {
        return false;
    };
    match drain_terminal_input_batch(&terminal) {
        Ok(backlog) => backlog,
        Err(()) => {
            debug!("TTY line discipline failed to drain UART input");
            false
        }
    }
}

/// @description 将指定 Terminal 的 raw input 送入 line discipline 并投递 foreground signals。
///
/// @param terminal console OFD 与 Process 共享的唯一 TTY owner。
/// @return drain 成功返回 `Ok(())`；设备或固定 queue 失败返回 `Err(())`。
pub(crate) fn drain_terminal_input(terminal: &crate::fs::Terminal) -> Result<(), ()> {
    drain_terminal_input_batch(terminal).map(|_| ())
}

fn drain_terminal_input_batch(terminal: &crate::fs::Terminal) -> Result<bool, ()> {
    let batch = terminal.drain_input().map_err(|_| ())?;
    super::publish_terminal_input_signals(terminal, batch.signals);
    Ok(batch.backlog)
}

/// @description 在统一 wait registry 中阻塞当前 console reader，封闭 read/enqueue IRQ race。
///
/// @param deadline VTIME 导出的 absolute monotonic deadline；无超时时为 None。
/// @param input_ready 在 registry owner lock 内复查 UART ring 的短闭包。
/// @return 输入已到达/IRQ 唤醒返回 `Woken`，到期返回 `TimedOut`，signal cancellation 返回 `Interrupted`。
pub(crate) fn wait_for_console(
    deadline: Option<u64>,
    input_ready: impl FnOnce() -> bool,
) -> WaitResult {
    let task = current_task().expect("console wait requires current task");
    let ticket = INDEXED_WAIT_QUEUE.lock().allocate_ticket();
    let prepared = ticket.prepare_console(deadline, task.clone());
    let queue = INDEXED_WAIT_QUEUE.lock();
    if input_ready() {
        return WaitResult::Woken;
    }
    if deadline.is_some_and(|value| value <= get_time_ns()) {
        return WaitResult::TimedOut;
    }
    if task.has_deliverable_signal() {
        return WaitResult::Interrupted;
    }
    let Ok(prepared) = prepared else {
        return WaitResult::OutOfMemory;
    };
    prepare_current_block(&task, queue, move |queue, _| {
        let wait_id = queue.commit(prepared);
        WaitMembership::Console(wait_id)
    })
    .suspend()
}

pub(super) fn wake_console_waiters() -> bool {
    const INPUT: i16 = 0x001;
    let mut waiters: [Option<(u64, VacantEntry<u64, wait_registry::IndexedWaitEntry>)>;
        console_batch::CONSOLE_WAKE_BATCH] = core::array::from_fn(|_| None);
    let mut batch = console_batch::ConsoleWakeBatch::new();
    let backlog = {
        let mut queue = INDEXED_WAIT_QUEUE.lock();
        while !batch.is_full() {
            let Some((wait_id, entry, group)) = queue.take_console(false, INPUT, batch.groups())
            else {
                break;
            };
            let slot = batch.selected();
            batch.record(group);
            waiters[slot] = Some((wait_id, entry));
        }
        if batch.is_full() {
            // 固定上限命中时续批；即使恰好摘完，也只多产生一个空批次。
            true
        } else {
            if let Some((wait_id, entry, group)) = queue.take_console(true, INPUT, batch.groups()) {
                let slot = batch.selected();
                batch.record(group);
                waiters[slot] = Some((wait_id, entry));
            }
            false
        }
    };
    // wake 会获取 scheduling/runqueue owner；固定数组保证全部 ownership 已在 registry 锁外。
    for (wait_id, entry) in waiters.into_iter().flatten() {
        let entry = entry.into_value();
        match entry.kind {
            IndexedWaitKind::Console => {
                let _ = crate::task::processor::wake_console_task(
                    entry.task,
                    wait_id,
                    WaitResult::Woken,
                );
            }
            IndexedWaitKind::Poll => {
                let _ =
                    crate::task::processor::wake_poll_task(entry.task, wait_id, WaitResult::Woken);
            }
            _ => panic!("console index contains non-console wait"),
        }
    }
    backlog
}
