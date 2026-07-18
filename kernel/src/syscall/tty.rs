use crate::{
    fs::{PtyMaster, Terminal, TerminalAccess},
    syscall::errno,
    task::{
        ProcessGroupError, TaskControlBlock, TerminalAccessError, check_terminal_access,
        claim_controlling_terminal, resize_terminal, set_terminal_foreground_group,
        terminal_foreground_group,
    },
};

use super::INTERNAL_RESTART_SYS;

const TCGETS: usize = 0x5401;
const TCSETS: usize = 0x5402;
const TCSETSW: usize = 0x5403;
const TCSETSF: usize = 0x5404;
const TIOCSCTTY: usize = 0x540e;
const TIOCGPGRP: usize = 0x540f;
const TIOCSPGRP: usize = 0x5410;
const TIOCGWINSZ: usize = 0x5413;
const TIOCSWINSZ: usize = 0x5414;
const TIOCGSID: usize = 0x5429;
const TIOCGPTN: usize = 0x8004_5430;
const TIOCSPTLCK: usize = 0x4004_5431;

/// @description 实现 Unix98 PTY master 专属 ioctl，并把通用 TTY request 投影到 slave。
/// @param task 当前 userspace address-space owner。
/// @param master `/dev/ptmx` OFD backend。
/// @param request Linux generic或 PTY master ioctl request。
/// @param argument request-specific userspace pointer/value。
/// @return 成功返回零；pointer/request 错误返回标准负 errno。
pub(super) fn pty_master_ioctl(
    task: &TaskControlBlock,
    master: &PtyMaster,
    request: usize,
    argument: usize,
) -> isize {
    match request {
        TIOCGPTN => task
            .copy_to_user(argument, &master.index().to_ne_bytes())
            .map_or(-errno::EFAULT, |()| 0),
        TIOCSPTLCK => {
            let mut bytes = [0u8; 4];
            if task.copy_from_user(argument, &mut bytes).is_err() {
                return -errno::EFAULT;
            }
            master.set_locked(i32::from_ne_bytes(bytes) != 0);
            0
        }
        _ => tty_ioctl(task, master.terminal(), request, argument),
    }
}

fn tty_error(error: ProcessGroupError) -> isize {
    match error {
        ProcessGroupError::NotFound => -errno::ESRCH,
        ProcessGroupError::Permission => -errno::EPERM,
        ProcessGroupError::NotTerminal => -errno::ENOTTY,
    }
}

/// @description 将 task-owned TTY job-control 结果翻译为 syscall 内部结果。
///
/// @param terminal 正在访问的 TTY owner。
/// @param access 输入、输出或状态修改。
/// @return 允许访问时成功；EIO 或内部 restart sentinel 时返回对应错误。
pub(super) fn guard_terminal_access(
    terminal: &Terminal,
    access: TerminalAccess,
) -> Result<(), isize> {
    check_terminal_access(terminal, access).map_err(|error| match error {
        TerminalAccessError::Io => -errno::EIO,
        TerminalAccessError::Restart => INTERNAL_RESTART_SYS,
    })
}

/// @description 实现唯一 Terminal OFD 的 Linux termios/session/foreground ioctl 子集。
///
/// @param fd 必须指向 Terminal OFD。
/// @param request Linux generic TTY ioctl request。
/// @param argument request-specific value 或用户指针。
/// @return 成功返回零；fd、用户地址、session/group 或 request 错误返回负 errno。
pub(super) fn tty_ioctl(
    task: &TaskControlBlock,
    terminal: &alloc::sync::Arc<Terminal>,
    request: usize,
    argument: usize,
) -> isize {
    match request {
        TCGETS => task
            .copy_to_user(argument, &terminal.termios())
            .map_or(-errno::EFAULT, |()| 0),
        TCSETS | TCSETSW | TCSETSF => {
            if let Err(error) = guard_terminal_access(terminal, TerminalAccess::StateChange) {
                return error;
            }
            let mut termios = [0u8; 36];
            if task.copy_from_user(argument, &mut termios).is_err() {
                return -errno::EFAULT;
            }
            match request {
                TCSETS => terminal.set_termios(termios),
                TCSETSW => terminal.set_termios_after_output(termios),
                TCSETSF => terminal.flush_input_and_set_termios(termios),
                _ => unreachable!(),
            }
            0
        }
        TIOCSCTTY => claim_controlling_terminal(terminal, argument).map_or_else(tty_error, |()| 0),
        TIOCGPGRP => match terminal_foreground_group(terminal) {
            Ok(pgid) => task
                .copy_to_user(argument, &(pgid as i32).to_ne_bytes())
                .map_or(-errno::EFAULT, |()| 0),
            Err(error) => tty_error(error),
        },
        TIOCSPGRP => {
            if let Err(error) = guard_terminal_access(terminal, TerminalAccess::StateChange) {
                return error;
            }
            let mut bytes = [0u8; 4];
            if task.copy_from_user(argument, &mut bytes).is_err() {
                return -errno::EFAULT;
            }
            let pgid = i32::from_ne_bytes(bytes);
            if pgid <= 0 {
                return -errno::EINVAL;
            }
            set_terminal_foreground_group(terminal, pgid as usize).map_or_else(tty_error, |()| 0)
        }
        TIOCGWINSZ => task
            .copy_to_user(argument, &terminal.window_size())
            .map_or(-errno::EFAULT, |()| 0),
        TIOCSWINSZ => {
            let mut window_size = [0u8; 8];
            if task.copy_from_user(argument, &mut window_size).is_err() {
                return -errno::EFAULT;
            }
            resize_terminal(terminal, window_size);
            0
        }
        TIOCGSID => match terminal.controlling_session() {
            Some(session) => task
                .copy_to_user(argument, &(session as i32).to_ne_bytes())
                .map_or(-errno::EFAULT, |()| 0),
            None => -errno::ENOTTY,
        },
        _ => -errno::ENOTTY,
    }
}
