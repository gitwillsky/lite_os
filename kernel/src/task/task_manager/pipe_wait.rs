use super::*;
use crate::ipc::{Pipe, PipeDirection, PipeEnd, PipeNotifier, PipeWaitCondition};

struct TaskPipeNotifier;

impl PipeNotifier for TaskPipeNotifier {
    fn notify(&self, pipe: &Arc<Pipe>) {
        crate::fs::Epoll::notify_pipe_source(pipe);
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
    let mut wake_groups = FallibleMap::new();
    let mut count = 0;
    'sources: for direction in [PipeDirection::Read, PipeDirection::Write] {
        // Read and write conditions are independent wait sources. Each direction may wake all
        // non-exclusive pollers plus one direct/EPOLLEXCLUSIVE registration.
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
        while let Some(wake) =
            WAIT_REGISTRY.wake_pipe_one(identity, direction, false, ready, state, &wake_groups)
        {
            let group_error = wake
                .group
                .is_some_and(|group| wake_groups.try_insert(group, ()).is_err());
            count += 1;
            wake_claimed_pipe(wake.claimed);
            if group_error {
                break 'sources;
            }
        }
        if let Some(wake) =
            WAIT_REGISTRY.wake_pipe_one(identity, direction, true, ready, state, &wake_groups)
        {
            let group_error = wake
                .group
                .is_some_and(|group| wake_groups.try_insert(group, ()).is_err());
            count += 1;
            wake_claimed_pipe(wake.claimed);
            if group_error {
                break 'sources;
            }
        }
    }
    count
}

fn wake_claimed_pipe(claimed: Option<wait_registry::ClaimedWait>) {
    if let Some(claimed) = claimed {
        match claimed.kind {
            IndexedWaitKind::Pipe { .. } => {
                crate::task::processor::wake_pipe_task(claimed.task, claimed.id, WaitResult::Woken);
            }
            IndexedWaitKind::Poll => {
                crate::task::processor::wake_poll_task(claimed.task, claimed.id, WaitResult::Woken);
            }
            _ => panic!("pipe index contains non-pipe wait"),
        }
    }
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
    let ticket = WAIT_REGISTRY.allocate_ticket();
    let prepared = ticket.prepare_pipe(pipe, condition, deadline, task.clone());
    arm_indexed_wait(
        &task,
        prepared,
        || {
            if pipe.wait_ready(condition) {
                Some(WaitResult::Woken)
            } else if deadline.is_some_and(|value| value <= crate::timer::get_time_ns()) {
                Some(WaitResult::TimedOut)
            } else if task.has_deliverable_signal() {
                Some(WaitResult::Interrupted)
            } else {
                None
            }
        },
        WaitMembership::Pipe,
    )
    .map_or_else(core::convert::identity, |prepared| prepared.suspend())
}
