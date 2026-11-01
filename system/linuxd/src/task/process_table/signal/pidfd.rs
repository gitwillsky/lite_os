//! pidfd 信号发送：固定 association、身份校验与 scope 路由，不重新按可复用数字 PID 选择。
use super::*;

/// native pidfd_send_signal 的 PID association scope；不重新按 visible number 选择目标。
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum PidSignalScope {
    Thread,
    ThreadGroup,
    ProcessGroup,
}

pub(crate) fn send_pid_signal(
    identity: &Arc<crate::namespace::PidIdentity>,
    scope: PidSignalScope,
    signal: usize,
    info: Option<PendingSignal>,
) -> Result<(), SignalSendError> {
    let sender = current_task().ok_or(SignalSendError::NotFound)?;
    if info.is_some_and(|info| info.restricted_user_source())
        && (scope == PidSignalScope::ProcessGroup || !Arc::ptr_eq(&sender.tid_identity(), identity))
    {
        return Err(SignalSendError::Permission);
    }
    {
        let graph = PROCESS_TABLE.graph.lock();
        let valid = match scope {
            PidSignalScope::Thread => {
                graph
                    .nodes
                    .get(&identity.key())
                    .is_some_and(|node| Arc::ptr_eq(node.pid.identity(), identity))
                    || graph
                        .threads
                        .get(&identity.key())
                        .is_some_and(|index| Arc::ptr_eq(index.pid.identity(), identity))
                    || graph
                        .kernel_threads
                        .get(&identity.key())
                        .is_some_and(|thread| Arc::ptr_eq(&thread.tid_identity(), identity))
            }
            PidSignalScope::ThreadGroup => {
                graph
                    .nodes
                    .get(&identity.key())
                    .is_some_and(|node| Arc::ptr_eq(node.pid.identity(), identity))
                    || graph
                        .threads
                        .get(&identity.key())
                        .is_some_and(|index| Arc::ptr_eq(index.pid.identity(), identity))
                    || graph
                        .kernel_threads
                        .get(&identity.key())
                        .is_some_and(|thread| Arc::ptr_eq(thread.tgid_identity(), identity))
            }
            PidSignalScope::ProcessGroup => graph
                .groups
                .get(&identity.key())
                .is_some_and(|group| Arc::ptr_eq(group.pgid.identity(), identity)),
        };
        if !valid {
            return Err(SignalSendError::NotFound);
        }
    }
    // stable graph key 永不复用；原 association 若在准备期间退休，既有 producer 返回 Gone。
    let origin = SignalOrigin::User { sender, info };
    match scope {
        PidSignalScope::Thread => send_selected_thread_signal(None, identity.key(), signal, origin),
        PidSignalScope::ThreadGroup => send_selected_processes(
            ProcessSelector::ThreadGroupOfPid(identity.key()),
            signal,
            origin,
        )
        .map(|_| ()),
        PidSignalScope::ProcessGroup => {
            send_selected_processes(ProcessSelector::Group(identity.key()), signal, origin)
                .map(|_| ())
        }
    }
}
