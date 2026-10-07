//! Linux termios/session/foreground 与 Unix98 PTY master ioctl 子集。

use alloc::sync::Arc;
use syscall_abi::{errno, signal};

use super::{PtyMaster, Terminal, TerminalAccess, fault, job_control};
use crate::fs::device::{DeviceError, IoctlCall};

const TCGETS: usize = 0x5401;
const TCSETS: usize = 0x5402;
const TCSETSW: usize = 0x5403;
const TCSETSF: usize = 0x5404;
const TCFLSH: usize = 0x540b;
const TIOCSCTTY: usize = 0x540e;
const TIOCGPGRP: usize = 0x540f;
const TIOCSPGRP: usize = 0x5410;
const TIOCGWINSZ: usize = 0x5413;
const TIOCSWINSZ: usize = 0x5414;
const TIOCGSID: usize = 0x5429;
const TIOCGPTN: usize = 0x8004_5430;
const TIOCSPTLCK: usize = 0x4004_5431;

/// Unix98 PTY master 专属 ioctl；其余 request 投影到 slave 的 Terminal。
///
/// # Errors
///
/// 用户地址、session/group 或 request 错误返回对应 errno。
pub(super) fn pty_master(master: &PtyMaster, call: &IoctlCall<'_>) -> Result<isize, DeviceError> {
    match call.request {
        TIOCGPTN => call
            .user
            .write(call.argument, &master.index().to_ne_bytes())
            .map(|()| 0)
            .map_err(fault),
        TIOCSPTLCK => {
            let mut bytes = [0u8; 4];
            call.user.read(call.argument, &mut bytes).map_err(fault)?;
            master.set_locked(i32::from_ne_bytes(bytes) != 0);
            Ok(0)
        }
        _ => terminal(master.terminal(), call),
    }
}

/// 一个 Terminal 的 Linux termios/session/foreground ioctl 子集。
///
/// 状态修改对任何打开方式都经过后台 process group 判定（Linux `tty_check_change`）；判定只对
/// 以该 Terminal 为 controlling TTY 的 session 生效。
///
/// # Errors
///
/// 用户地址、session/group 或 request 错误返回对应 errno；后台修改按 job control 返回
/// `EIO` 或 `Restart`。
pub(super) fn terminal(
    terminal: &Arc<Terminal>,
    call: &IoctlCall<'_>,
) -> Result<isize, DeviceError> {
    let (user, argument) = (call.user, call.argument);
    let check_change = || job_control().check_access(terminal, TerminalAccess::StateChange);
    match call.request {
        TCGETS => user.write(argument, &terminal.termios()).map_err(fault)?,
        TCSETS | TCSETSW | TCSETSF => {
            check_change()?;
            let mut termios = [0u8; 36];
            user.read(argument, &mut termios).map_err(fault)?;
            match call.request {
                TCSETS => terminal.set_termios(termios),
                TCSETSW => terminal.set_termios_after_output(termios),
                _ => terminal.flush_input_and_set_termios(termios),
            }
        }
        TCFLSH => {
            check_change()?;
            let (input, output) = match argument {
                0 => (true, false),
                1 => (false, true),
                2 => (true, true),
                _ => return Err(DeviceError::Errno(errno::EINVAL)),
            };
            terminal.flush(input, output);
        }
        TIOCSCTTY => job_control().claim_controlling(terminal, argument)?,
        TIOCGPGRP => {
            let pgid = job_control().foreground_group(terminal)?;
            user.write(argument, &(pgid as i32).to_ne_bytes())
                .map_err(fault)?;
        }
        TIOCSPGRP => {
            check_change()?;
            let mut bytes = [0u8; 4];
            user.read(argument, &mut bytes).map_err(fault)?;
            let pgid = i32::from_ne_bytes(bytes);
            if pgid <= 0 {
                return Err(DeviceError::Errno(errno::EINVAL));
            }
            job_control().set_foreground_group(terminal, pgid as usize)?;
        }
        TIOCGWINSZ => user
            .write(argument, &terminal.window_size())
            .map_err(fault)?,
        // Linux tty_do_resize：尺寸变化时通知 foreground group。
        TIOCSWINSZ => {
            let mut window_size = [0u8; 8];
            user.read(argument, &mut window_size).map_err(fault)?;
            if let Some(pgid) = terminal.set_window_size(window_size) {
                job_control().signal_group(pgid, signal::SIGWINCH);
            }
        }
        TIOCGSID => {
            let session = terminal
                .controlling_session()
                .ok_or(DeviceError::Errno(errno::ENOTTY))?;
            user.write(argument, &(session as i32).to_ne_bytes())
                .map_err(fault)?;
        }
        _ => return Err(DeviceError::Errno(errno::ENOTTY)),
    }
    Ok(0)
}
