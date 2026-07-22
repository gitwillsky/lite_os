//! Owned PTY session creation and cleanup.

use std::{
    ffi::CString,
    ffi::OsString,
    fs::File,
    io::{self, Read, Write},
    os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd},
    os::unix::process::CommandExt,
    process::{Child, Command, Stdio},
};

use crate::raw;

#[derive(Clone, Copy)]
pub struct WindowSize {
    pub columns: u16,
    pub rows: u16,
    pub pixel_width: u16,
    pub pixel_height: u16,
}

pub struct PtySession {
    master: File,
    child: Child,
}

impl PtySession {
    /// Spawns one explicitly selected command as the PTY session leader.
    ///
    /// # Parameters
    ///
    /// - `size`: Initial terminal grid and pixel geometry.
    /// - `program`: Exact executable path; no shell lookup or default is applied.
    /// - `arguments`: Exact argv entries after argv[0].
    ///
    /// # Returns
    ///
    /// The unique PTY master and child-process owner.
    ///
    /// # Errors
    ///
    /// Returns the first PTY, ioctl, fork or exec setup error.
    pub fn spawn(size: WindowSize, program: &OsString, arguments: &[OsString]) -> io::Result<Self> {
        let path = CString::new("/dev/ptmx").expect("static PTY path");
        let master_raw = unsafe {
            raw::open(
                path.as_ptr(),
                raw::O_RDWR | raw::O_NONBLOCK | raw::O_CLOEXEC,
                0,
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
        let slave_raw = unsafe { raw::open(slave_path.as_ptr(), raw::O_RDWR | raw::O_CLOEXEC, 0) };
        if slave_raw < 0 {
            return Err(io::Error::last_os_error());
        }
        let slave = unsafe { OwnedFd::from_raw_fd(slave_raw) };
        set_window_size(master.as_fd(), size)?;
        let parent = std::process::id() as i32;
        let slave_fd = slave.as_raw_fd();
        let stdin = File::from(slave.try_clone()?);
        let stdout = File::from(slave.try_clone()?);
        let stderr = File::from(slave);
        let mut command = Command::new(program);
        command
            .args(arguments)
            .env_clear()
            .env("TERM", "liteos")
            .env("HOME", "/root")
            .env("PATH", "/sbin:/usr/sbin:/bin:/usr/bin")
            .stdin(Stdio::from(stdin))
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr));
        // `Command` owns the fork/exec boundary, so spawning remains valid when a future
        // application has multiple threads. The child callback performs only raw syscalls.
        unsafe {
            command.pre_exec(move || {
                if raw::prctl(raw::PR_SET_PDEATHSIG, raw::SIGKILL) < 0 {
                    return Err(io::Error::last_os_error());
                }
                if raw::getppid() != parent {
                    return Err(io::Error::from_raw_os_error(raw::ECHILD));
                }
                if raw::setsid() < 0
                    || raw::ioctl(slave_fd, raw::TIOCSCTTY, std::ptr::null_mut()) < 0
                {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn()?;
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

    pub fn terminate(&mut self) -> io::Result<()> {
        let signal = unsafe { raw::kill(-(self.child.id() as i32), raw::SIGKILL) };
        let wait = self.child.wait().map(|_| ());
        if signal < 0 {
            Err(io::Error::last_os_error())
        } else {
            wait
        }
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
