use super::*;
use crate::ipc::{Pipe, PipeDirection, PipeEnd, PipeNotifier, PipeWaitCondition};

struct TaskPipeNotifier;

impl PipeNotifier for TaskPipeNotifier {
    fn notify(&self, pipe: &Arc<Pipe>) {
        wake_pipe_waiters(pipe);
    }
}

fn create_endpoints(
    pair: impl FnOnce(Arc<dyn PipeNotifier>) -> Result<(Arc<PipeEnd>, Arc<PipeEnd>), ()>,
) -> Result<(Arc<PipeEnd>, Arc<PipeEnd>), ()> {
    let notifier = Arc::try_new(TaskPipeNotifier).map_err(|_| ())?;
    pair(notifier)
}

/// @description 创建绑定统一 task wait registry 的 64 KiB data Pipe endpoints。
/// @return anonymous pipe、AF_UNIX transport 与 PTY output 使用的 read/write endpoints。
pub(crate) fn create_pipe_endpoints() -> Result<(Arc<PipeEnd>, Arc<PipeEnd>), ()> {
    create_endpoints(Pipe::pair)
}

/// @description 创建绑定同一 task wait registry 的一字节 notification Pipe endpoints。
/// @return DRM/input/PTY/epoll/eventfd/socket readiness 使用的 read/write token endpoints。
pub(crate) fn create_notification_endpoints() -> Result<(Arc<PipeEnd>, Arc<PipeEnd>), ()> {
    create_endpoints(Pipe::notification_pair)
}

fn wake_pipe_waiters(pipe: &Arc<Pipe>) -> usize {
    const INPUT: i16 = 0x001;
    const OUTPUT: i16 = 0x004;
    const ERROR: i16 = 0x008;
    const HANGUP: i16 = 0x010;
    let identity = Pipe::identity(pipe);
    let mut waiters = FallibleMap::new();
    let mut wake_groups = FallibleMap::new();
    let mut exclusive_selected = false;
    let mut queue = INDEXED_WAIT_QUEUE.lock();
    'sources: for direction in [PipeDirection::Read, PipeDirection::Write] {
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
        while let Some((entry, group)) =
            queue.take_pipe(identity, direction, false, ready, state, &wake_groups)
        {
            if let Some(group) = group
                && wake_groups.try_insert(group, ()).is_err()
            {
                waiters.commit_vacant(entry);
                break 'sources;
            }
            waiters.commit_vacant(entry);
        }
        if !exclusive_selected
            && let Some((entry, group)) =
                queue.take_pipe(identity, direction, true, ready, state, &wake_groups)
        {
            if let Some(group) = group
                && wake_groups.try_insert(group, ()).is_err()
            {
                waiters.commit_vacant(entry);
                break 'sources;
            }
            exclusive_selected = true;
            waiters.commit_vacant(entry);
        }
    }
    drop(queue);
    let count = waiters.len();
    let mut waiters = waiters;
    while let Some((&wait_id, _)) = waiters.first_key_value() {
        let entry = waiters.remove(&wait_id).expect("staged pipe waiter");
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
    wait_for_pipe_until(pipe, condition, None)
}

/// @description 阻塞到 pipe 条件满足、absolute deadline 到期或 signal interruption。
/// @param pipe anonymous pipe owner。
/// @param condition read data/EOF 或 write capacity/broken peer。
/// @param deadline 可选 absolute monotonic 纳秒 deadline。
/// @return ready、timeout、signal 或 wait publication OOM。
pub(crate) fn wait_for_pipe_until(
    pipe: &Arc<Pipe>,
    condition: PipeWaitCondition,
    deadline: Option<u64>,
) -> WaitResult {
    let task = current_task().expect("pipe wait requires current task");
    let mut queue = INDEXED_WAIT_QUEUE.lock();
    if pipe.wait_ready(condition) {
        return WaitResult::Woken;
    }
    if deadline.is_some_and(|value| value <= crate::timer::get_time_ns()) {
        return WaitResult::TimedOut;
    }
    if task.has_deliverable_signal() {
        return WaitResult::Interrupted;
    }
    let Ok(prepared) = queue.prepare_pipe(pipe, condition, deadline, task.clone()) else {
        return WaitResult::OutOfMemory;
    };
    super::context_switch::prepare_current_block(&task, queue, move |queue, _| {
        let wait_id = queue.commit(prepared);
        WaitMembership::Pipe(wait_id)
    })
    .suspend()
}
