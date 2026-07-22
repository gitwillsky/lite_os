//! Process-session operations not represented by [`std::process`].

use std::{
    io,
    os::unix::process::CommandExt,
    process::{Child, ChildStdin, ChildStdout, Command, ExitStatus},
};

use crate::raw;

/// Result of creating a background copy of a single-threaded process.
pub enum Fork {
    Parent { child: Pid },
    Child,
}

/// Forks the current process before it has created any threads.
///
/// The caller must immediately return from the parent without running resource
/// destructors whose underlying kernel objects are intentionally inherited by
/// the child. This narrow interface exists for the boot splash handoff only.
pub fn fork_background() -> io::Result<Fork> {
    let result = unsafe { raw::fork() };
    match result {
        result if result < 0 => Err(io::Error::last_os_error()),
        0 => Ok(Fork::Child),
        child => Ok(Fork::Parent { child: Pid(child) }),
    }
}

/// A positive Linux process identifier.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Pid(i32);

impl Pid {
    pub fn new(raw: i32) -> Option<Self> {
        (raw > 0).then_some(Self(raw))
    }

    pub fn get(self) -> i32 {
        self.0
    }
}

/// Signals used by the product userspace process supervisors.
#[derive(Clone, Copy)]
pub enum Signal {
    Kill,
    Terminate,
}

impl Signal {
    fn raw(self) -> i32 {
        match self {
            Self::Kill => raw::SIGKILL,
            Self::Terminate => raw::SIGTERM,
        }
    }
}

/// Sends a signal to one process.
pub fn signal(pid: Pid, signal: Signal) -> io::Result<()> {
    if unsafe { raw::kill(pid.0, signal.raw()) } < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// A child that owns its process session and is killed and reaped on drop.
pub struct SessionChild {
    child: Child,
}

impl SessionChild {
    /// Spawns `command` in a new session with a parent-death kill signal.
    pub fn spawn(command: &mut Command) -> io::Result<Self> {
        let parent = std::process::id() as i32;
        unsafe {
            command.pre_exec(move || {
                if raw::prctl(raw::PR_SET_PDEATHSIG, raw::SIGKILL) < 0 {
                    return Err(io::Error::last_os_error());
                }
                if raw::getppid() != parent {
                    return Err(io::Error::from_raw_os_error(raw::ECHILD));
                }
                if raw::setsid() < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        command.spawn().map(|child| Self { child })
    }

    pub fn id(&self) -> Pid {
        Pid(self.child.id() as i32)
    }

    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }

    /// Takes the child's piped standard input.
    ///
    /// # Returns
    ///
    /// The unique writer when the command requested `Stdio::piped`, otherwise `None`.
    pub fn take_stdin(&mut self) -> Option<ChildStdin> {
        self.child.stdin.take()
    }

    /// Takes the child's piped standard output.
    ///
    /// # Returns
    ///
    /// The unique reader when the command requested `Stdio::piped`, otherwise `None`.
    pub fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.child.stdout.take()
    }

    pub fn terminate(&mut self) -> io::Result<()> {
        let pid = self.id().0;
        let signal_result = unsafe { raw::kill(-pid, raw::SIGKILL) };
        let wait_result = self.child.wait().map(|_| ());
        if signal_result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::NotFound {
                return Err(error);
            }
        }
        wait_result
    }
}

impl Drop for SessionChild {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.terminate();
        }
    }
}
