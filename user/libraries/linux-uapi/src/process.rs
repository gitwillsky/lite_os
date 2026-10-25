//! Process-session operations not represented by [`std::process`].

use std::{
    ffi::{OsStr, OsString},
    io,
    io::{Read, Write},
    os::{
        fd::{AsFd, AsRawFd, OwnedFd},
        unix::process::CommandExt,
    },
    process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Stdio},
};

use crate::raw;

/// 子进程的实际用户和主组；指定后标准 Command 会清空继承的补充组。
/// 这是执行身份，不代表调用方已经通过认证；认证策略由会话应用拥有。
#[derive(Debug, Clone, Copy)]
pub struct Identity {
    pub uid: u32,
    pub gid: u32,
}

/// 以 Wayland 约定拉起特权客户端，通过标准输入传递已连接的 socket。
///
/// # Parameters
///
/// - `program`: 要执行的程序；继承环境，`WAYLAND_SOCKET` 设为标准输入的编号 0。
/// - `socket`: 客户端连接；交给 `Command` 的标准 I/O 装配，返回后父进程不再持有。
/// - `identity`: Some 时使用标准 Command 的 gid/uid 装配并清除补充组；None 继承父进程身份。
///
/// # Returns
///
/// 可等待回收的子进程。该客户端的标准输入专用于 Wayland，不用于终端输入。
///
/// # Errors
///
/// 程序不存在、不可执行或标准 I/O 装配失败。
pub fn spawn_with_wayland_socket(
    program: &OsStr,
    socket: OwnedFd,
    identity: Option<Identity>,
) -> io::Result<Child> {
    let mut command = Command::new(program);
    command
        .env("WAYLAND_SOCKET", "0")
        .stdin(Stdio::from(socket));
    if let Some(identity) = identity {
        command.gid(identity.gid).uid(identity.uid);
    }
    command.spawn()
}

/// 复制 `WAYLAND_SOCKET` 指定的连接，并为原描述符与副本设置 `FD_CLOEXEC`。
///
/// 保留环境变量，避免修改多线程进程环境；启动普通应用的调用者须用
/// `Command::env_remove("WAYLAND_SOCKET")`，否则应用可能尝试使用已被 exec 关闭的描述符。
/// 原描述符仍归进程启动时的继承状态所有，返回的连接拥有独立的描述符。
///
/// # Returns
///
/// 未设置 `WAYLAND_SOCKET` 或非 Linux 目标时返回 `Ok(None)`。
///
/// # Errors
///
/// 环境变量不是非负描述符编号、描述符没有打开或复制失败。
pub fn wayland_socket_from_env() -> io::Result<Option<std::os::unix::net::UnixStream>> {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::{FromRawFd, RawFd};
        const F_GETFD: i32 = 1;
        const F_SETFD: i32 = 2;
        const F_DUPFD_CLOEXEC: i32 = 1030;
        const FD_CLOEXEC: i32 = 1;
        let Some(value) = std::env::var_os("WAYLAND_SOCKET") else {
            return Ok(None);
        };
        let descriptor: RawFd = value
            .to_str()
            .and_then(|text| text.parse().ok())
            .filter(|descriptor| *descriptor >= 0)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "WAYLAND_SOCKET is not a descriptor number",
                )
            })?;
        // SAFETY: fcntl 只查询/设置内核描述符标志，不借用用户内存；无效描述符返回 errno。
        let flags = unsafe { raw::fcntl(descriptor, F_GETFD, 0) };
        // SAFETY: 保留已有标志，阻止继承的连接跨过后续 exec；不关闭可能仍被标准输入借用的 fd。
        if flags < 0 || unsafe { raw::fcntl(descriptor, F_SETFD, flags | FD_CLOEXEC) } < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: 内核原子分配新的 CLOEXEC fd，不接管环境变量中可能已有其他 owner 的 fd。
        let duplicate = unsafe { raw::fcntl(descriptor, F_DUPFD_CLOEXEC, 0) };
        if duplicate < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: duplicate 是刚分配且未发布的有效 fd，唯一所有权交给返回的 UnixStream。
        Ok(Some(unsafe {
            std::os::unix::net::UnixStream::from_raw_fd(duplicate)
        }))
    }
    #[cfg(not(target_os = "linux"))]
    {
        Ok(None)
    }
}

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
    /// 完成 group kill/wait 后不再 signal 相同编号，防止 Drop 二次终止已复用的进程组。
    terminated: bool,
}

/// Exact standard-I/O shape supported by a supervised graphical-session child.
pub enum SessionIo {
    /// Null standard input/output and target diagnostics on `/dev/console`.
    Background,
    /// Piped standard input/output and target diagnostics on `/dev/console`.
    Piped,
    /// 消费 Wayland stdin 与双向监督 stdout socket；stderr 为控制台，不与 exec-status 混用。
    Wayland { socket: OwnedFd, control: OwnedFd },
}

/// A checked command that can only be launched through the session trampoline.
pub struct SessionCommand {
    program: OsString,
    arguments: Vec<OsString>,
    io: SessionIo,
    user: Option<SessionUser>,
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
            user: None,
        }
    }
}

/// 已认证账户的执行环境；logon 独占账户策略，launcher 仅实施标准凭据切换。
#[derive(Debug, Clone)]
pub struct SessionUser {
    pub identity: Identity,
    pub groups: Vec<u32>,
    pub name: String,
    pub home: std::path::PathBuf,
    pub runtime: std::path::PathBuf,
}
impl SessionCommand {
    /// 指定认证后的用户、补充组与清洁环境；消费后只能通过 session trampoline 执行。
    pub fn user(mut self, user: SessionUser) -> Self {
        self.user = Some(user);
        self
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
            .stderr(Stdio::piped());
        if let Some(user) = command.user {
            launch
                .env_clear()
                .env(
                    "PATH",
                    "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
                )
                .env("LANG", "zh_CN.UTF-8")
                .env("HOME", &user.home)
                .env("USER", &user.name)
                .env("LOGNAME", &user.name)
                .env("XDG_RUNTIME_DIR", &user.runtime)
                .env("WAYLAND_DISPLAY", "/run/liteos/wayland-0")
                .current_dir(&user.home);
            launch
                .arg("--user")
                .arg(user.identity.uid.to_string())
                .arg(user.identity.gid.to_string())
                .arg(
                    user.groups
                        .iter()
                        .map(u32::to_string)
                        .collect::<Vec<_>>()
                        .join(","),
                );
        }
        launch
            .arg("--")
            .arg(&command.program)
            .args(&command.arguments);
        match command.io {
            SessionIo::Background => {
                launch.stdin(Stdio::null()).stdout(Stdio::null());
            }
            SessionIo::Piped => {
                launch.stdin(Stdio::piped()).stdout(Stdio::piped());
            }
            SessionIo::Wayland { socket, control } => {
                launch
                    .env("WAYLAND_SOCKET", "0")
                    .stdin(Stdio::from(socket))
                    .stdout(Stdio::from(control));
            }
        }
        let mut child = launch.spawn()?;
        let mut status = child
            .stderr
            .take()
            .ok_or_else(|| io::Error::other("session exec-status pipe missing"))?;
        let error = match read_exec_status(&mut status) {
            Ok(()) => {
                return Ok(Self {
                    child,
                    terminated: false,
                });
            }
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
        if self.terminated {
            return Ok(());
        }
        let pid = self.id().0;
        // Capture errno before wait: libc calls during reaping may overwrite the signal error.
        // SAFETY: pid is the checked leader identity of this owned session; kill takes no pointers.
        let signal_error = if unsafe { raw::kill(-pid, raw::SIGKILL) } < 0 {
            Some(io::Error::last_os_error())
        } else {
            None
        };
        let wait_result = self.child.wait().map(|_| ());
        if let Some(error) = signal_error {
            // ESRCH denotes an absent process group; ErrorKind mapping varies between host OSes.
            if error.raw_os_error() != Some(raw::ESRCH) {
                return Err(error);
            }
        }
        if wait_result.is_ok() {
            self.terminated = true;
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
/// - `user`: 可选的 uid/gid 与完整补充组；console 打开后、parent-death 设置前降权。
///
/// # Returns
///
/// This function never returns. Success replaces the current image with
/// `program`; setup or `exec` failure is published to [`SessionChild::spawn`]
/// and exits with status 127.
pub fn exec_session_child(
    expected_parent: Pid,
    program: &OsStr,
    arguments: &[OsString],
    user: Option<(Identity, Vec<u32>)>,
) -> ! {
    let mut publication = match std::io::stderr().as_fd().try_clone_to_owned() {
        Ok(file) => std::fs::File::from(file),
        Err(error) => {
            eprintln!("session-launch: exec-status capture failed: {error}");
            std::process::exit(127);
        }
    };
    let console = match std::fs::OpenOptions::new().write(true).open("/dev/console") {
        Ok(console) => console,
        Err(error) => report_exec_failure(&mut publication, 4, error),
    };
    if unsafe { raw::dup2(console.as_raw_fd(), 2) } < 0 {
        report_exec_failure(&mut publication, 5, io::Error::last_os_error());
    }
    if let Some((identity, groups)) = user {
        set_identity(&mut publication, identity, &groups);
    }
    // 先降权再设置 parent-death：setuid 会清除此前设置的信号，顺序反过来会留下孤儿 desktop。
    enter_session(&mut publication, expected_parent);
    let error = Command::new(program).args(arguments).exec();
    report_exec_failure(&mut publication, 6, error);
}

#[cfg(target_os = "linux")]
fn set_identity(publication: &mut std::fs::File, identity: Identity, groups: &[u32]) {
    // SAFETY: trampoline 是尚未建立线程的独立进程；groups 切片在系统调用期间有效。
    if unsafe { raw::setgroups(groups.len(), groups.as_ptr()) } < 0
        || unsafe { raw::setgid(identity.gid) } < 0
        || unsafe { raw::setuid(identity.uid) } < 0
    {
        report_exec_failure(publication, 6, io::Error::last_os_error());
    }
}
#[cfg(not(target_os = "linux"))]
fn set_identity(publication: &mut std::fs::File, _identity: Identity, _groups: &[u32]) {
    report_exec_failure(publication, 6, io::ErrorKind::Unsupported.into());
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
        // leader 已退出也必须清理仍继承会话进程组的应用；只回收 leader 会漏掉后台应用。
        let _ = self.terminate();
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
        let mut session = SessionChild {
            child,
            terminated: false,
        };
        session.terminate().unwrap();
        assert_eq!(
            signal(pid, Signal::Kill).unwrap_err().raw_os_error(),
            Some(raw::ESRCH)
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn terminate_reaps_successfully_when_the_group_already_exited() {
        let mut child = Command::new("setsid")
            .args(["/bin/sh", "-c", "exit 0"])
            .spawn()
            .unwrap();
        assert!(child.wait().unwrap().success());
        let mut session = SessionChild {
            child,
            terminated: false,
        };
        session.terminate().unwrap();
        assert!(session.try_wait().unwrap().is_some());
    }
}
