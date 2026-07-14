use super::*;
use crate::ipc::{Pipe, PipeDirection, PipeEnd, PipeNotifier, PipeWaitCondition};

struct TaskPipeNotifier;

impl PipeNotifier for TaskPipeNotifier {
    fn notify(&self, pipe: &Arc<Pipe>) {
        wake_pipe_waiters(pipe);
    }
}

/// @description 创建绑定统一 task wait registry 的 anonymous pipe endpoints。
///
/// @return read/write endpoints；kernel heap 不足返回错误。
pub(crate) fn create_pipe_endpoints() -> Result<(Arc<PipeEnd>, Arc<PipeEnd>), ()> {
    Pipe::pair(Arc::new(TaskPipeNotifier))
}

fn wake_pipe_waiters(pipe: &Arc<Pipe>) -> usize {
    const INPUT: i16 = 0x001;
    const OUTPUT: i16 = 0x004;
    const ERROR: i16 = 0x008;
    const HANGUP: i16 = 0x010;
    let identity = Pipe::identity(pipe);
    let mut waiters = alloc::vec::Vec::new();
    let mut wake_groups = BTreeSet::new();
    let mut exclusive_selected = false;
    let mut queue = INDEXED_WAIT_QUEUE.lock();
    for direction in [PipeDirection::Read, PipeDirection::Write] {
        let state = pipe.poll_state(direction);
        let ready = match direction {
            PipeDirection::Read => {
                (if state.readable { INPUT } else { 0 }) | if state.hangup { HANGUP } else { 0 }
            }
            PipeDirection::Write => {
                (if state.writable { OUTPUT } else { 0 }) | if state.error { ERROR } else { 0 }
            }
        };
        if ready == 0 {
            continue;
        }
        while let Some((wait_id, entry, group)) =
            queue.take_pipe(identity, direction, false, ready, state, &wake_groups)
        {
            if let Some(group) = group {
                wake_groups.insert(group);
            }
            waiters.push((wait_id, entry));
        }
        if !exclusive_selected
            && let Some((wait_id, entry, group)) =
                queue.take_pipe(identity, direction, true, ready, state, &wake_groups)
        {
            if let Some(group) = group {
                wake_groups.insert(group);
            }
            exclusive_selected = true;
            waiters.push((wait_id, entry));
        }
    }
    drop(queue);
    let count = waiters.len();
    for (wait_id, entry) in waiters {
        match entry.kind {
            IndexedWaitKind::Pipe { .. } => {
                crate::task::processor::wake_pipe_task(entry.task, wait_id, WaitResult::Woken);
            }
            IndexedWaitKind::Poll => {
                crate::task::processor::wake_poll_task(entry.task, wait_id, WaitResult::Woken);
            }
            _ => panic!("pipe index contains non-pipe wait"),
        }
    }
    count
}

/// @description 在统一 wait registry 阻塞到精确 pipe I/O 条件成立或 signal interruption。
///
/// @param pipe anonymous pipe owner。
/// @param condition read 等待 data/EOF；write 等待完整原子写容量/broken reader。
/// @return ready 返回 Woken；signal 返回 Interrupted。
pub(crate) fn wait_for_pipe(pipe: &Arc<Pipe>, condition: PipeWaitCondition) -> WaitResult {
    let task = current_task().expect("pipe wait requires current task");
    let queue = INDEXED_WAIT_QUEUE.lock();
    if pipe.wait_ready(condition) {
        return WaitResult::Woken;
    }
    if task.has_deliverable_signal() {
        return WaitResult::Interrupted;
    }
    super::context_switch::prepare_current_block(&task, queue, |queue, current| {
        let wait_id = queue.insert_pipe(pipe, condition, current);
        WaitMembership::Pipe(wait_id)
    })
    .suspend()
}
