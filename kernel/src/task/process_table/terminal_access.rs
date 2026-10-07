use crate::{
    fs::device::DeviceError,
    tty::{JobControl, Terminal, TerminalAccess},
};
use syscall_abi::errno;

use super::*;

/// task 为 fs TTY 提供的唯一 job-control 实现。
struct TaskJobControl;

// OWNER: 无状态的唯一 job-control 实现；由 `install_job_control` 安装进 tty。缺失时 tty 无法
// 判定后台访问或投递 SIGTTIN/SIGTTOU/SIGHUP/SIGWINCH 与 ISIG。
static TASK_JOB_CONTROL: TaskJobControl = TaskJobControl;

/// 在任何 TTY open 之前把 job control 安装到 fs。
pub(in crate::task) fn install_job_control() {
    crate::tty::install_job_control(&TASK_JOB_CONTROL);
}

fn process_group_error(error: ProcessGroupError) -> DeviceError {
    DeviceError::Errno(match error {
        ProcessGroupError::NotFound => errno::ESRCH,
        ProcessGroupError::Permission => errno::EPERM,
        ProcessGroupError::NotTerminal => errno::ENOTTY,
    })
}

impl JobControl for TaskJobControl {
    /// 1. 一次 process graph 快照取得 caller session、process group 与 POSIX orphan 状态；
    /// 2. Terminal 判定是否需要 SIGTTIN/SIGTTOU；
    /// 3. signal 被阻塞或忽略时 SIGTTIN 返回 `EIO`、SIGTTOU 放行；孤儿 group 返回 `EIO`；
    ///    否则向 caller group 投递 signal 并要求 syscall 重启。
    fn check_access(&self, terminal: &Terminal, access: TerminalAccess) -> Result<(), DeviceError> {
        let task = current_task().expect("TTY access requires current task");
        let (session, process_group, orphaned) = {
            let graph = PROCESS_TABLE.graph.lock();
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
            return if signal == syscall_abi::signal::SIGTTIN {
                Err(DeviceError::Errno(errno::EIO))
            } else {
                Ok(())
            };
        }
        if orphaned {
            return Err(DeviceError::Errno(errno::EIO));
        }
        assert_ne!(
            send_process_group_signal(process_group, signal),
            0,
            "current TTY process group disappeared"
        );
        Err(DeviceError::Restart)
    }

    fn signal_group(&self, pgid: usize, signal: usize) {
        send_process_group_signal(pgid, signal);
    }

    fn wait_for_console(
        &self,
        deadline: Option<u64>,
        input_ready: &dyn Fn() -> bool,
    ) -> WaitResult {
        super::console_wait::wait_for_console(deadline, input_ready)
    }

    fn controlling_terminal(&self) -> Option<Arc<Terminal>> {
        let task = current_task()?;
        let session = session_id(0).ok()?;
        let terminal = task.terminal();
        (terminal.controlling_session() == Some(session)).then_some(terminal)
    }

    fn claim_controlling(&self, terminal: &Arc<Terminal>, force: usize) -> Result<(), DeviceError> {
        claim_controlling_terminal(terminal, force).map_err(process_group_error)
    }

    fn foreground_group(&self, terminal: &Terminal) -> Result<usize, DeviceError> {
        terminal_foreground_group(terminal).map_err(process_group_error)
    }

    fn set_foreground_group(&self, terminal: &Terminal, pgid: usize) -> Result<(), DeviceError> {
        set_terminal_foreground_group(terminal, pgid).map_err(process_group_error)
    }
}
