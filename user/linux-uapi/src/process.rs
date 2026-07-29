//! Process-session operations not represented by [`std::process`].

use std::{
    ffi::{OsStr, OsString},
    io,
    io::{Read, Write},
    os::{
        fd::{AsFd, AsRawFd},
        unix::process::CommandExt,
    },
    process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Stdio},
};

use crate::raw;

const SESSION_LAUNCHER: &str = "/bin/session-launch";
const EXEC_STATUS_HEADER: [u8; 5] = *b"LSEX\x01";
const EXEC_STATUS_ERROR_LEN: usize = 10;
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

/// Exact standard-I/O shape supported by a supervised graphical-session child.
#[derive(Clone, Copy)]
pub enum SessionIo {
    /// Null standard input/output and target diagnostics on `/dev/console`.
    Background,
    /// Piped standard input/output and target diagnostics on `/dev/console`.
    Piped,
}

/// A checked command that can only be launched through the session trampoline.
pub struct SessionCommand {
    program: OsString,
    arguments: Vec<OsString>,
    io: SessionIo,
}

impl SessionCommand {
    /// Creates one supervised command.
    ///
    /// # Parameters
    ///
    /// - `program`: Absolute executable path passed to `exec`.
    /// - `arguments`: Argument vector after `argv[0]`.
    /// - `io`: Exact supported standard-I/O profile.
    ///
    /// # Returns
    ///
    /// An unstarted command consumed by [`SessionChild::spawn`].
    pub fn new(program: impl Into<OsString>, arguments: Vec<OsString>, io: SessionIo) -> Self {
        Self {
            program: program.into(),
            arguments,
            io,
        }
    }
}

impl SessionChild {
    /// Spawns a command through the single-threaded session trampoline.
    ///
    /// # Parameters
    ///
    /// - `command`: Checked program, arguments and standard-I/O profile.
    ///
    /// # Returns
    ///
    /// The child after the CLOEXEC status pipe reaches empty EOF without a
    /// deterministic setup or `exec` error publication.
    ///
    /// # Errors
    ///
    /// Returns spawn, trampoline setup, or target `exec` errors after reaping
    /// the failed child. The multi-threaded caller never runs `pre_exec` or raw
    /// `fork`.
    pub fn spawn(command: SessionCommand) -> io::Result<Self> {
        if command.program.is_empty() || !std::path::Path::new(&command.program).is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "session program must be a non-empty absolute path",
            ));
        }
        let mut launch = Command::new(SESSION_LAUNCHER);
        launch
            .arg(std::process::id().to_string())
            .arg("--")
            .arg(&command.program)
            .args(&command.arguments)
            .stderr(Stdio::piped());
        match command.io {
            SessionIo::Background => {
                launch.stdin(Stdio::null()).stdout(Stdio::null());
            }
            SessionIo::Piped => {
                launch.stdin(Stdio::piped()).stdout(Stdio::piped());
            }
        }
        let mut child = launch.spawn()?;
        let mut status = child
            .stderr
            .take()
            .ok_or_else(|| io::Error::other("session exec-status pipe missing"))?;
        let error = match read_exec_status(&mut status) {
            Ok(()) => return Ok(Self { child }),
            Err(error) => error,
        };
        let _ = child.kill();
        let _ = child.wait();
        Err(error)
    }

    /// Returns the process/session/group identity owned by this child.
    ///
    /// # Returns
    ///
    /// The positive PID that also names the child's session and process group.
    pub fn id(&self) -> Pid {
        Pid(self.child.id() as i32)
    }

    /// Observes child exit without blocking.
    ///
    /// # Returns
    ///
    /// `None` while the child is alive or its cached exit status after exit.
    ///
    /// # Errors
    ///
    /// Returns the standard `wait` observation error.
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

    /// Kills the complete owned process group and synchronously reaps its leader.
    ///
    /// # Returns
    ///
    /// `Ok(())` after the session leader has been reaped.
    ///
    /// # Errors
    ///
    /// Returns a group-signal error other than an already-absent group, or the
    /// standard blocking `wait` error.
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

/// Replaces the single-threaded trampoline with one supervised session child.
///
/// # Parameters
///
/// - `expected_parent`: Parent identity captured before the trampoline spawn.
/// - `program`: Target executable path.
/// - `arguments`: Target arguments after `argv[0]`.
///
/// # Returns
///
/// This function never returns. Success replaces the current image with
/// `program`; setup or `exec` failure is published to [`SessionChild::spawn`]
/// and exits with status 127.
pub fn exec_session_child(expected_parent: Pid, program: &OsStr, arguments: &[OsString]) -> ! {
    let mut publication = match std::io::stderr().as_fd().try_clone_to_owned() {
        Ok(file) => std::fs::File::from(file),
        Err(error) => {
            eprintln!("session-launch: exec-status capture failed: {error}");
            std::process::exit(127);
        }
    };
    enter_session(&mut publication, expected_parent);
    let console = match std::fs::OpenOptions::new().write(true).open("/dev/console") {
        Ok(console) => console,
        Err(error) => report_exec_failure(&mut publication, 4, error),
    };
    if unsafe { raw::dup2(console.as_raw_fd(), 2) } < 0 {
        report_exec_failure(&mut publication, 5, io::Error::last_os_error());
    }
    let error = Command::new(program).args(arguments).exec();
    report_exec_failure(&mut publication, 6, error);
}

#[cfg(target_os = "linux")]
fn raw_parent_pid() -> Pid {
    Pid(unsafe { raw::getppid() })
}

#[cfg(target_os = "linux")]
fn enter_session(publication: &mut std::fs::File, expected_parent: Pid) {
    if unsafe { raw::prctl(raw::PR_SET_PDEATHSIG, raw::SIGKILL) } < 0 {
        report_exec_failure(publication, 1, io::Error::last_os_error());
    }
    if raw_parent_pid() != expected_parent {
        report_exec_failure(publication, 2, io::Error::from_raw_os_error(raw::ECHILD));
    }
    if unsafe { raw::setsid() } < 0 {
        report_exec_failure(publication, 3, io::Error::last_os_error());
    }
}

#[cfg(not(target_os = "linux"))]
fn enter_session(publication: &mut std::fs::File, _expected_parent: Pid) {
    report_exec_failure(
        publication,
        1,
        io::Error::new(
            io::ErrorKind::Unsupported,
            "session trampoline requires Linux process controls",
        ),
    );
}

fn report_exec_failure(publication: &mut std::fs::File, stage: u8, error: io::Error) -> ! {
    let mut frame = [0u8; EXEC_STATUS_ERROR_LEN];
    frame[..EXEC_STATUS_HEADER.len()].copy_from_slice(&EXEC_STATUS_HEADER);
    frame[5] = stage;
    frame[6..].copy_from_slice(&error.raw_os_error().unwrap_or(5).to_ne_bytes());
    let _ = publication.write_all(&frame);
    std::process::exit(127);
}

fn read_exec_status(status: &mut impl Read) -> io::Result<()> {
    let mut publication = [0u8; EXEC_STATUS_ERROR_LEN + 1];
    let mut length = 0;
    loop {
        if length == publication.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "session trampoline exceeded the exec-status frame",
            ));
        }
        match status.read(&mut publication[length..]) {
            Ok(0) => break,
            Ok(count) => length += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    let publication = &publication[..length];
    if publication.is_empty() {
        return Ok(());
    }
    if publication.len() != EXEC_STATUS_ERROR_LEN
        || publication[..EXEC_STATUS_HEADER.len()] != EXEC_STATUS_HEADER
        || !(1..=6).contains(&publication[5])
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "session trampoline truncated the exec-status protocol",
        ));
    }
    let raw_errno = i32::from_ne_bytes(
        publication[6..]
            .try_into()
            .expect("checked exec-error frame"),
    );
    if raw_errno <= 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "session trampoline returned an invalid errno",
        ));
    }
    let stage = publication[5];
    let source = io::Error::from_raw_os_error(raw_errno);
    Err(io::Error::new(
        source.kind(),
        format!("session trampoline stage {stage}: {source}"),
    ))
}

impl Drop for SessionChild {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.terminate();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    #[test]
    fn exec_status_is_bounded_and_rejects_partial_or_invalid_frames() {
        assert!(read_exec_status(&mut Cursor::new([])).is_ok());
        let mut frame = [0u8; EXEC_STATUS_ERROR_LEN];
        frame[..5].copy_from_slice(&EXEC_STATUS_HEADER);
        frame[5] = 6;
        frame[6..].copy_from_slice(&2i32.to_ne_bytes());
        for length in 1..EXEC_STATUS_ERROR_LEN {
            assert_eq!(
                read_exec_status(&mut Cursor::new(&frame[..length]))
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::InvalidData
            );
        }
        assert_eq!(
            read_exec_status(&mut Cursor::new([0u8; EXEC_STATUS_ERROR_LEN + 1]))
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        frame[6..].copy_from_slice(&0i32.to_ne_bytes());
        assert_eq!(
            read_exec_status(&mut Cursor::new(frame))
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn exec_status_preserves_stage_and_errno() {
        let mut frame = [0u8; EXEC_STATUS_ERROR_LEN];
        frame[..5].copy_from_slice(&EXEC_STATUS_HEADER);
        frame[5] = 6;
        frame[6..].copy_from_slice(&2i32.to_ne_bytes());
        let error = read_exec_status(&mut Cursor::new(frame)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(error.to_string().contains("stage 6"));
    }

    #[test]
    fn session_command_rejects_non_absolute_program_before_spawn() {
        let result = SessionChild::spawn(SessionCommand::new(
            "relative",
            Vec::new(),
            SessionIo::Background,
        ));
        let error = match result {
            Ok(_) => panic!("relative program unexpectedly spawned"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn terminate_kills_and_reaps_the_owned_process_group() {
        let mut child = Command::new("setsid")
            .args(["/bin/sh", "-c", "printf R; sleep 30 & wait"])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut ready = [0u8; 1];
        child
            .stdout
            .as_mut()
            .unwrap()
            .read_exact(&mut ready)
            .unwrap();
        assert_eq!(ready, *b"R");
        let pid = Pid::new(child.id() as i32).unwrap();
        let mut session = SessionChild { child };
        session.terminate().unwrap();
        assert_eq!(
            signal(pid, Signal::Kill).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }
}
