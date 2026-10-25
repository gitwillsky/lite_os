//! Owned PTY session creation and cleanup.

use std::{
    ffi::CString,
    ffi::OsString,
    fs::File,
    io::{self, Read, Write},
    os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd},
};

use crate::{
    process::{SessionChild, SessionCommand, SessionIo},
    raw,
};

const SESSION_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
const SESSION_HOME: &str = "/root";
const SESSION_SHELL: &str = "/bin/sh";
const SESSION_USER: &str = "root";

#[derive(Clone, Copy)]
pub struct WindowSize {
    pub columns: u16,
    pub rows: u16,
    pub pixel_width: u16,
    pub pixel_height: u16,
}

pub struct PtySession {
    master: File,
    child: SessionChild,
}

impl PtySession {
    /// Spawns one explicitly selected command as the PTY session leader.
    ///
    /// # Parameters
    ///
    /// - `size`: Initial terminal grid and pixel geometry.
    /// - `program`: Exact executable path; no shell lookup or default is applied.
    /// - `arguments`: Exact argv entries after argv[0].
    /// - `environment`: Application-owned variables added after the fixed
    ///   terminal client environment.
    ///
    /// # Returns
    ///
    /// The unique PTY master and child-process owner.
    ///
    /// # Errors
    ///
    /// Returns the first PTY, ioctl, fork or exec setup error.
    /// Non-Linux child setup returns `Unsupported`: it cannot provide the Linux
    /// controlling-terminal and parent-death contract.
    pub fn spawn(
        size: WindowSize,
        program: &OsString,
        arguments: &[OsString],
        environment: &[(OsString, OsString)],
    ) -> io::Result<Self> {
        let path = CString::new("/dev/ptmx").expect("static PTY path");
        let master_raw = unsafe {
            raw::open(
                path.as_ptr(),
                raw::O_RDWR | raw::O_NONBLOCK | raw::O_CLOEXEC,
            )
        };
        if master_raw < 0 {
            return Err(io::Error::last_os_error());
        }
        let master = unsafe { OwnedFd::from_raw_fd(master_raw) };
        let mut index = 0u32;
        let mut unlocked = 0i32;
        ioctl(master.as_raw_fd(), raw::TIOCGPTN, (&raw mut index).cast())?;
        ioctl(
            master.as_raw_fd(),
            raw::TIOCSPTLCK,
            (&raw mut unlocked).cast(),
        )?;
        let slave_path = CString::new(format!("/dev/pts/{index}"))
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid PTY path"))?;
        // 1. 调用者可能是 session leader（经 session trampoline 启动）；没有 O_NOCTTY 时，
        //    打开 slave 会把它变成调用者自己的 controlling terminal。
        // 2. 随后子进程 setsid 后的 TIOCSCTTY 因 tty 已属于另一个 session 而失败，spawn 返回
        //    错误并丢弃 master；PTY hangup 向前台组（即调用者自己）投递 SIGHUP，调用者在
        //    报告错误前被杀死，终端窗口只剩空白。
        let slave_raw = unsafe {
            raw::open(
                slave_path.as_ptr(),
                raw::O_RDWR | raw::O_NOCTTY | raw::O_CLOEXEC,
            )
        };
        if slave_raw < 0 {
            return Err(io::Error::last_os_error());
        }
        let slave = unsafe { OwnedFd::from_raw_fd(slave_raw) };
        set_window_size(master.as_fd(), size)?;
        let home = std::env::var_os("HOME").unwrap_or_else(|| SESSION_HOME.into());
        let mut values = vec![
            ("TERM".into(), "linux".into()),
            ("HOME".into(), home.clone()),
            ("SHELL".into(), SESSION_SHELL.into()),
            (
                "USER".into(),
                std::env::var_os("USER").unwrap_or_else(|| SESSION_USER.into()),
            ),
            (
                "PATH".into(),
                std::env::var_os("PATH").unwrap_or_else(|| SESSION_PATH.into()),
            ),
        ];
        values.extend_from_slice(environment);
        let child = SessionChild::spawn(
            SessionCommand::new(
                program.clone(),
                arguments.to_vec(),
                SessionIo::Pty { slave },
            )
            .environment(values)
            .directory(home.into()),
        )?;
        Ok(Self {
            master: File::from(master),
            child,
        })
    }

    pub fn as_fd(&self) -> BorrowedFd<'_> {
        self.master.as_fd()
    }

    pub fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        self.master.read(output)
    }

    pub fn write(&mut self, input: &[u8]) -> io::Result<usize> {
        self.master.write(input)
    }

    pub fn resize(&self, size: WindowSize) -> io::Result<()> {
        set_window_size(self.master.as_fd(), size)
    }

    /// Reports whether the session leader has exited (reaping it on the first positive answer).
    pub fn has_exited(&mut self) -> io::Result<bool> {
        Ok(self.child.try_wait()?.is_some())
    }

    pub fn terminate(&mut self) -> io::Result<()> {
        self.child.terminate()
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        let _ = self.terminate();
    }
}

fn set_window_size(fd: BorrowedFd<'_>, size: WindowSize) -> io::Result<()> {
    let mut raw_size = raw::WindowSize {
        rows: size.rows,
        columns: size.columns,
        pixel_width: size.pixel_width,
        pixel_height: size.pixel_height,
    };
    ioctl(fd.as_raw_fd(), raw::TIOCSWINSZ, (&raw mut raw_size).cast())
}

fn ioctl(fd: i32, request: usize, argument: *mut std::ffi::c_void) -> io::Result<()> {
    if unsafe { raw::ioctl(fd, request, argument) } < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{SESSION_HOME, SESSION_PATH, SESSION_SHELL, SESSION_USER};

    #[test]
    fn pty_path_matches_login_profile() {
        assert_eq!(
            include_str!("../../../base/profile").trim(),
            format!("export PATH={SESSION_PATH}")
        );
    }

    #[test]
    fn pty_exposes_the_installed_posix_shell() {
        assert_eq!(SESSION_HOME, "/root");
        assert_eq!(SESSION_SHELL, "/bin/sh");
        assert_eq!(SESSION_USER, "root");
    }
}
