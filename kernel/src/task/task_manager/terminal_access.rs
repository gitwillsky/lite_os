use crate::fs::{Terminal, TerminalAccess};

use super::*;

/// @description TTY job-control access check 的领域错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TerminalAccessError {
    Io,
    Restart,
}

/// @description 执行 PTY master close 的 controlling-terminal hangup consequence。
/// @param terminal 正在失去 master endpoint 的唯一 Terminal owner。
/// @return 无返回值；没有 foreground process group 时幂等完成。
pub(crate) fn hangup_terminal(terminal: &Terminal) {
    let Some(pgid) = terminal.hangup() else {
        return;
    };
    send_process_group_signal(pgid, 1);
    send_process_group_signal(pgid, 18);
}

/// @description 提交 TTY window size，并按 Linux tty resize 语义通知 foreground group。
/// @param terminal `TIOCSWINSZ` fd 指向的唯一 Terminal owner。
/// @param window_size 已完整 copy-in 的 Linux `struct winsize` bytes。
/// @return 无返回值；尺寸未变化或无 foreground group 时幂等完成。
pub(crate) fn resize_terminal(terminal: &Terminal, window_size: [u8; 8]) {
    if let Some(pgid) = terminal.set_window_size(window_size) {
        send_process_group_signal(pgid, 28);
    }
}

/// @description 对 controlling TTY 后台访问执行唯一 job-control 判定与 signal generation。
///
/// @param terminal caller 正在访问的 TTY owner。
/// @param access 输入、输出或 TTY 状态修改。
/// @return foreground、非 controlling TTY 或允许的后台输出返回成功。
/// @errors blocked/ignored SIGTTIN 或 orphaned group 返回 `Io`；已发布 SIGTTIN/SIGTTOU 返回 `Restart`。
pub(crate) fn check_terminal_access(
    terminal: &Terminal,
    access: TerminalAccess,
) -> Result<(), TerminalAccessError> {
    let task = current_task().expect("TTY access requires current task");
    let (session, process_group, orphaned) = {
        let graph = TASK_MANAGER.graph.lock();
        let node = graph
            .nodes
            .get(&task.tgid())
            .expect("TTY caller missing from process graph");
        let session = node.session;
        let process_group = node.process_group;
        let orphaned = !graph.nodes.values().any(|member| {
            member.session == session
                && member.process_group == process_group
                && matches!(member.state, ProcessState::Live(_))
                && member.parent.is_some_and(|parent| {
                    graph.nodes.get(&parent).is_some_and(|parent| {
                        parent.session == session && parent.process_group != process_group
                    })
                })
        });
        (session, process_group, orphaned)
    };
    let Some(signal) = terminal.background_signal(session, process_group, access) else {
        return Ok(());
    };
    let mask = task
        .signal_mask(0, None)
        .expect("signal mask query cannot fail");
    let action = task
        .signal_action(signal, None)
        .expect("TTY job-control signal must be valid");
    let blocked_or_ignored = mask & (1u64 << (signal - 1)) != 0 || action.handler == 1;
    if blocked_or_ignored {
        return if signal == 21 {
            Err(TerminalAccessError::Io)
        } else {
            Ok(())
        };
    }
    if orphaned {
        return Err(TerminalAccessError::Io);
    }
    assert_ne!(
        send_process_group_signal(process_group, signal),
        0,
        "current TTY process group disappeared"
    );
    Err(TerminalAccessError::Restart)
}
