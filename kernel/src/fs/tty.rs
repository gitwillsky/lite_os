//! Linux TTY 字符设备：`/dev/tty`、`/dev/console`、`/dev/ptmx` 与 `/dev/pts/N`。
//!
//! line discipline 归 [`Terminal`]，Unix98 pair 生命周期归 `pty`；这里把它们投影为注册表中的
//! 设备文件，并拥有读写阻塞、poll 唤醒源与 termios/session ioctl。session、process group 与
//! signal 归 task，经 [`install_job_control`] 安装的 [`JobControl`] 访问。

use alloc::sync::Arc;
use spin::Once;
use syscall_abi::{errno, signal};

use super::{
    Console, FileSystemError, PtyMaster, PtySlave, Terminal, TerminalAccess, TerminalRead,
    TerminalReadMode, character_write_chunk,
    device::{
        self, CharacterDriver, DeviceError, DeviceFile, DeviceNumber, DeviceWaitSource,
        DeviceWaitSources, IoctlCall, OpenRequest, UserFault, UserInput, UserOutput,
    },
    pty,
};
use crate::{
    drivers::console::ConsoleDevice,
    ipc::{Pipe, PipeDirection, PipeRead, PipeWaitCondition},
    sync::WaitResult,
};

mod ioctl;

const POLLIN: i16 = 0x001;
const POLLOUT: i16 = 0x004;
const POLLHUP: i16 = 0x010;
/// 单次 terminal read 的 kernel 中转上限。
const TERMINAL_READ_BYTES: usize = 512;

/// Linux TTYAUX major：`tty`、`console`、`ptmx` 依次为 minor 0、1、2。
const TTYAUX_MAJOR: u32 = 5;
const TTY_MINOR: u32 = 0;
const CONSOLE_MINOR: u32 = 1;
const PTMX_MINOR: u32 = 2;
const CONSOLE_NUMBER: DeviceNumber = DeviceNumber::new(TTYAUX_MAJOR, CONSOLE_MINOR);
/// Linux `UNIX98_PTY_SLAVE_MAJOR`；slave index 直接作为 minor。
pub(super) const PTS_MAJOR: u32 = 136;
/// pts minor 空间（Linux `MINORBITS`）；pty index 不得超出。
pub(super) const PTS_MINOR_COUNT: u32 = 1 << 20;

/// task 为 TTY 提供的 job-control 能力；fs 不依赖 task，由 task 初始化时安装。
pub(crate) trait JobControl: Sync {
    /// 对调用者访问 `terminal` 执行 Linux `job_control`/`tty_check_change` 判定，必要时向调用者
    /// process group 投递 SIGTTIN/SIGTTOU。
    ///
    /// # Errors
    ///
    /// SIGTTIN 被阻塞/忽略或 group 已孤儿化返回 `EIO`；已投递 signal 返回 `Restart`。
    fn check_access(&self, terminal: &Terminal, access: TerminalAccess) -> Result<(), DeviceError>;

    /// 向 process group 投递 signal；group 已不存在时无操作。
    fn signal_group(&self, pgid: usize, signal: usize);

    /// 阻塞到 `input_ready` 为真、`deadline` 到期或可交付 signal。
    fn wait_for_console(&self, deadline: Option<u64>, input_ready: &dyn Fn() -> bool)
    -> WaitResult;

    /// 调用者 session 的 controlling terminal；没有时为 `None`。
    fn controlling_terminal(&self) -> Option<Arc<Terminal>>;

    /// `TIOCSCTTY`：调用者 session leader 取得 `terminal`。
    ///
    /// # Errors
    ///
    /// 非 session leader、`force` 非零或 terminal 属于其他 session 返回 `EPERM`。
    fn claim_controlling(&self, terminal: &Arc<Terminal>, force: usize) -> Result<(), DeviceError>;

    /// `TIOCGPGRP`。
    ///
    /// # Errors
    ///
    /// terminal 不是调用者 session 的 controlling TTY 返回 `ENOTTY`。
    fn foreground_group(&self, terminal: &Terminal) -> Result<usize, DeviceError>;

    /// `TIOCSPGRP`。
    ///
    /// # Errors
    ///
    /// group 不存在或跨 session 返回 `EPERM`；terminal 不属于调用者 session 返回 `ENOTTY`。
    fn set_foreground_group(&self, terminal: &Terminal, pgid: usize) -> Result<(), DeviceError>;
}

// OWNER: 唯一 job-control 实现由 task 在 `task::initialize` 安装，早于任何 TTY open。缺失时
// 后台访问、ISIG 与 hangup 无法投递 signal，只能 fail-stop。
static JOB_CONTROL: Once<&'static dyn JobControl> = Once::new();

// OWNER: 系统 console 的唯一 Terminal；`/dev/console`、init 的 fd 0/1/2 与 deferred UART 输入
// 共享它。缺失时每个进程会各自持有一个 console line discipline，输入被任意一个吞掉。
static CONSOLE: Once<Arc<Terminal>> = Once::new();

/// 安装 task 提供的 job-control 实现。
///
/// # Panics
///
/// 重复安装时 panic。
pub(crate) fn install_job_control(control: &'static dyn JobControl) {
    assert!(JOB_CONTROL.get().is_none(), "job control installed twice");
    JOB_CONTROL.call_once(|| control);
}

fn job_control() -> &'static dyn JobControl {
    *JOB_CONTROL
        .get()
        .expect("TTY used before task installed job control")
}

/// 把 console 设备投影为 Terminal 的 raw byte 设备。
struct DeviceConsole(Arc<dyn ConsoleDevice>);

impl Console for DeviceConsole {
    fn read(&self, bytes: &mut [u8]) -> Result<usize, FileSystemError> {
        Ok(self.0.read(bytes))
    }

    fn input_ready(&self) -> bool {
        self.0.input_ready()
    }

    fn discard_input(&self) -> usize {
        self.0.discard_input()
    }

    fn discard_output(&self) -> usize {
        0
    }

    fn write(&self, bytes: &[u8]) -> Result<usize, FileSystemError> {
        self.0
            .write(bytes)
            .map(|()| bytes.len())
            .map_err(|_| FileSystemError::IoError)
    }
}

/// 以 `console` 创建系统 console Terminal 并注册全部 TTY 设备。
///
/// # Returns
///
/// console 就绪的证明；init 的 fd 0/1/2 经 `/dev/console` 打开，需要它。
///
/// # Errors
///
/// 重复初始化返回 `AlreadyExists`；分配失败返回 `OutOfMemory`。
pub(crate) fn init(
    console: Arc<dyn ConsoleDevice>,
) -> Result<super::ConsoleReady, FileSystemError> {
    if CONSOLE.get().is_some() {
        return Err(FileSystemError::AlreadyExists);
    }
    let console = Arc::try_new(DeviceConsole(console)).map_err(|_| FileSystemError::OutOfMemory)?;
    let terminal =
        Terminal::new(console, CONSOLE_NUMBER).map_err(|()| FileSystemError::OutOfMemory)?;
    pty::init()?;
    CONSOLE.call_once(|| terminal);
    let auxiliary = Arc::try_new(TtyAuxiliaryDriver).map_err(|_| FileSystemError::OutOfMemory)?;
    device::register_driver(DeviceNumber::new(TTYAUX_MAJOR, 0), 3, auxiliary)?;
    for (path, minor, permissions) in [
        (&b"tty"[..], TTY_MINOR, 0o666),
        (b"console", CONSOLE_MINOR, 0o600),
        (b"ptmx", PTMX_MINOR, 0o666),
    ] {
        device::register_node(path, DeviceNumber::new(TTYAUX_MAJOR, minor), permissions)?;
    }
    let slaves = Arc::try_new(PtsDriver).map_err(|_| FileSystemError::OutOfMemory)?;
    device::register_driver(DeviceNumber::new(PTS_MAJOR, 0), PTS_MINOR_COUNT, slaves)?;
    Ok(super::ConsoleReady(()))
}

/// 系统 console 的 Terminal。
///
/// # Panics
///
/// [`init`] 之前调用时 panic。
pub(crate) fn console() -> Arc<Terminal> {
    CONSOLE
        .get()
        .cloned()
        .expect("console terminal used before TTY initialization")
}

/// 把 terminal raw input 送入 line discipline，并向 foreground group 投递生成的 ISIG signals。
///
/// # Returns
///
/// raw input 仍有未消费 backlog 时为 true。
///
/// # Errors
///
/// line discipline 或底层 console 失败返回对应错误。
pub(crate) fn drain_input(terminal: &Terminal) -> Result<bool, FileSystemError> {
    let batch = terminal.drain_input()?;
    deliver_input_signals(terminal, batch.signals);
    Ok(batch.backlog)
}

/// 把 line discipline 生成的 ISIG bitset 投递给 foreground process group。
pub(super) fn deliver_input_signals(terminal: &Terminal, signals: u64) {
    if signals == 0 {
        return;
    }
    let Some(pgid) = terminal.signal_target_group() else {
        return;
    };
    for number in 1..=64 {
        if signals & (1u64 << (number - 1)) != 0 {
            job_control().signal_group(pgid, number);
        }
    }
}

/// PTY master 关闭的 controlling-terminal hangup：脱离 session 并投递 SIGHUP/SIGCONT。
pub(super) fn hangup(terminal: &Terminal) {
    if let Some(pgid) = terminal.hangup() {
        job_control().signal_group(pgid, signal::SIGHUP);
        job_control().signal_group(pgid, signal::SIGCONT);
    }
}

fn fault(_: UserFault) -> DeviceError {
    DeviceError::Errno(errno::EFAULT)
}

fn terminal_error(error: FileSystemError) -> DeviceError {
    DeviceError::Errno(match error {
        FileSystemError::OutOfMemory => errno::ENOMEM,
        _ => errno::EIO,
    })
}

/// 无 deadline 等待的结果；被 signal 中断返回 `EINTR`。
fn waited(result: WaitResult) -> Result<(), DeviceError> {
    match result {
        WaitResult::Woken | WaitResult::TimedOut => Ok(()),
        WaitResult::Interrupted => Err(DeviceError::Errno(errno::EINTR)),
        WaitResult::OutOfMemory => Err(DeviceError::Errno(errno::ENOMEM)),
    }
}

/// `tty`/`console`/`ptmx` 的 driver。
struct TtyAuxiliaryDriver;

impl CharacterDriver for TtyAuxiliaryDriver {
    fn open(&self, request: &OpenRequest<'_>) -> Result<Arc<dyn DeviceFile>, FileSystemError> {
        match request.number.minor {
            // `/dev/tty` 是调用者 controlling terminal 的别名：按其实际设备重新打开，读写与唤醒
            // 源因此与直接打开该设备一致。
            TTY_MINOR => {
                let terminal = job_control()
                    .controlling_terminal()
                    .ok_or(FileSystemError::NoDevice)?;
                open_terminal(terminal.device_number(), true)
            }
            CONSOLE_MINOR => open_terminal(CONSOLE_NUMBER, false),
            PTMX_MINOR => {
                let master = pty::open_master(request.identity.uid(), request.identity.gid())?;
                Arc::try_new(PtyMasterFile(master))
                    .map(|file| file as Arc<dyn DeviceFile>)
                    .map_err(|_| FileSystemError::OutOfMemory)
            }
            _ => Err(FileSystemError::NoDevice),
        }
    }
}

/// `/dev/pts/N` 的 driver。
struct PtsDriver;

impl CharacterDriver for PtsDriver {
    fn open(&self, request: &OpenRequest<'_>) -> Result<Arc<dyn DeviceFile>, FileSystemError> {
        open_terminal(request.number, true)
    }
}

/// 打开 console 或一个 pts 的 terminal 文件。
///
/// `job_control` 为 false 时读写不做后台访问判定，保持 `/dev/console` 的系统输出不受 TOSTOP
/// 约束；termios 修改始终判定。
fn open_terminal(
    number: DeviceNumber,
    job_control: bool,
) -> Result<Arc<dyn DeviceFile>, FileSystemError> {
    let file = if number == CONSOLE_NUMBER {
        TerminalFile {
            terminal: console(),
            pty: None,
            job_control,
        }
    } else if number.major == PTS_MAJOR {
        let slave = pty::open_slave(number.minor)?;
        TerminalFile {
            terminal: slave.terminal().clone(),
            pty: Some(slave),
            job_control,
        }
    } else {
        return Err(FileSystemError::NoDevice);
    };
    Arc::try_new(file)
        .map(|file| file as Arc<dyn DeviceFile>)
        .map_err(|_| FileSystemError::OutOfMemory)
}

/// 一个打开的 console 或 pts。
struct TerminalFile {
    terminal: Arc<Terminal>,
    /// pts 的 slave 生命周期引用；console 为 `None`。
    pty: Option<Arc<PtySlave>>,
    /// 读写是否经过后台 process group 判定。
    job_control: bool,
}

impl TerminalFile {
    fn check_access(&self, access: TerminalAccess) -> Result<(), DeviceError> {
        if self.job_control {
            job_control().check_access(&self.terminal, access)
        } else {
            Ok(())
        }
    }
}

impl DeviceFile for TerminalFile {
    /// 按 termios 的 canonical/VMIN/VTIME 读取 line discipline 输入。
    fn read(&self, output: &mut dyn UserOutput, nonblocking: bool) -> Result<(), DeviceError> {
        let terminal = &self.terminal;
        let mut input = [0u8; TERMINAL_READ_BYTES];
        let capacity = output.remaining().min(input.len());
        let mode = terminal.read_mode(capacity);
        let mut read = 0;
        // VTIME 在 MIN=0 时从 read 开始计时，在 MIN>0 时从首字节开始并按后续每批输入重置；
        // 缺少这个区分会让 curses halfdelay 永久阻塞。
        let mut deadline = match mode {
            TerminalReadMode::Noncanonical {
                minimum: 0,
                timeout_ns,
            } if timeout_ns != 0 => Some(crate::timer::get_time_ns().saturating_add(timeout_ns)),
            _ => None,
        };
        loop {
            self.check_access(TerminalAccess::Input)?;
            drain_input(terminal).map_err(|_| DeviceError::Errno(errno::EIO))?;
            match terminal.read(&mut input[read..capacity]) {
                TerminalRead::Empty => {
                    if self
                        .pty
                        .as_ref()
                        .is_some_and(|slave| slave.master_hung_up())
                    {
                        break;
                    }
                    if matches!(
                        mode,
                        TerminalReadMode::Noncanonical {
                            minimum: 0,
                            timeout_ns: 0,
                        }
                    ) {
                        break;
                    }
                    if nonblocking {
                        if read == 0 {
                            return Err(DeviceError::WouldBlock);
                        }
                        break;
                    }
                    let wait = match &self.pty {
                        Some(slave) => match slave.prepare_to_block() {
                            None => WaitResult::Woken,
                            Some(pipe) => pipe.wait(PipeWaitCondition::Readable, deadline),
                        },
                        None => job_control().wait_for_console(deadline, &|| terminal.wait_ready()),
                    };
                    match wait {
                        WaitResult::Woken => continue,
                        WaitResult::Interrupted if read == 0 => {
                            return Err(DeviceError::Errno(errno::EINTR));
                        }
                        WaitResult::Interrupted | WaitResult::TimedOut => break,
                        WaitResult::OutOfMemory if read == 0 => {
                            return Err(DeviceError::Errno(errno::ENOMEM));
                        }
                        WaitResult::OutOfMemory => break,
                    }
                }
                TerminalRead::Bytes(count) => {
                    read += count;
                    match mode {
                        TerminalReadMode::Canonical => break,
                        TerminalReadMode::Noncanonical {
                            minimum,
                            timeout_ns,
                        } => {
                            if read == capacity || read >= minimum {
                                break;
                            }
                            if timeout_ns != 0 {
                                deadline =
                                    Some(crate::timer::get_time_ns().saturating_add(timeout_ns));
                            }
                        }
                    }
                }
                TerminalRead::Eof => break,
            }
        }
        output.write(&input[..read]).map_err(fault)
    }

    /// 经 line discipline 输出；pts 在 master 端队列满时只在尚未写出任何字节时阻塞。
    fn write(&self, input: &mut dyn UserInput, nonblocking: bool) -> Result<(), DeviceError> {
        self.check_access(TerminalAccess::Output)?;
        let mut buffer = [0u8; TERMINAL_READ_BYTES];
        let mut progressed = false;
        while input.remaining() != 0 {
            let requested = character_write_chunk(input.remaining(), false);
            input.copy(&mut buffer[..requested]).map_err(fault)?;
            let count = match &self.pty {
                Some(slave) => loop {
                    match slave.write(&buffer[..requested]) {
                        Ok(0) if progressed => return Ok(()),
                        Ok(0) if nonblocking => return Err(DeviceError::WouldBlock),
                        Ok(0) => waited(slave.output_pipe().wait(
                            PipeWaitCondition::Writable {
                                minimum: PtySlave::output_write_minimum(requested),
                            },
                            None,
                        ))?,
                        Ok(count) => break count,
                        Err(error) => return Err(terminal_error(error)),
                    }
                },
                None => self
                    .terminal
                    .write(&buffer[..requested])
                    .map_err(terminal_error)?,
            };
            input.consume(count);
            progressed = true;
            if count < requested {
                break;
            }
        }
        Ok(())
    }

    fn poll(&self, events: i16) -> i16 {
        let hung_up = self
            .pty
            .as_ref()
            .is_some_and(|slave| slave.master_hung_up());
        let output = if hung_up {
            POLLHUP
        } else if self
            .pty
            .as_ref()
            .is_none_or(|slave| slave.output_writable())
        {
            events & POLLOUT
        } else {
            0
        };
        let input = if self.terminal.input_ready() {
            events & POLLIN
        } else {
            0
        };
        output | input
    }

    fn wait_sources(&self, events: i16) -> DeviceWaitSources {
        let Some(slave) = &self.pty else {
            let mut sources = DeviceWaitSources::new();
            sources.push(DeviceWaitSource::Console);
            return sources;
        };
        let mut sources = DeviceWaitSources::pipe(slave.notification_pipe(), POLLIN);
        if events & POLLOUT != 0 {
            sources.push(DeviceWaitSource::Pipe {
                pipe: slave.output_pipe(),
                direction: PipeDirection::Write,
                events,
            });
        }
        sources
    }

    fn readiness_generation(&self) -> u64 {
        self.pty.as_ref().map_or_else(
            || self.terminal.readiness_generation(),
            |slave| slave.readiness_generation(),
        )
    }

    /// pts 走 notification pipe 协议；console 没有 Pipe 源，只在 poll 注册前把 UART raw input
    /// 推进 line discipline，阻塞读经 [`JobControl::wait_for_console`] 等待。
    fn prepare_wait(&self, _events: i16) -> Option<Arc<Pipe>> {
        match &self.pty {
            Some(slave) => slave.prepare_to_block(),
            None => {
                let _ = drain_input(&self.terminal);
                None
            }
        }
    }

    fn ioctl(&self, call: &IoctlCall<'_>) -> Result<isize, DeviceError> {
        ioctl::terminal(&self.terminal, call)
    }
}

/// 一个打开的 `/dev/ptmx`。
struct PtyMasterFile(Arc<PtyMaster>);

impl DeviceFile for PtyMasterFile {
    /// 读取 slave 输出字节流；slave 全部关闭后返回 `EIO`（Linux pty master 语义）。
    fn read(&self, output: &mut dyn UserOutput, nonblocking: bool) -> Result<(), DeviceError> {
        let master = &self.0;
        let mut buffer = [0u8; TERMINAL_READ_BYTES];
        let mut progressed = false;
        while output.remaining() != 0 {
            let requested = output.remaining().min(buffer.len());
            // 读出会消费 Pipe 字节；先证明目标可写，避免 fault 丢弃已出队输出。
            output.reserve(requested).map_err(fault)?;
            let read = loop {
                match master.read(&mut buffer[..requested]) {
                    PipeRead::Bytes(count) => break count,
                    PipeRead::Eof => return Err(DeviceError::Errno(errno::EIO)),
                    PipeRead::Empty if master.peer_hung_up() => {
                        return Err(DeviceError::Errno(errno::EIO));
                    }
                    PipeRead::Empty if progressed => return Ok(()),
                    PipeRead::Empty if nonblocking => return Err(DeviceError::WouldBlock),
                    PipeRead::Empty => {
                        if let Some(pipe) = master.prepare_to_block() {
                            waited(pipe.wait(PipeWaitCondition::Readable, None))?;
                        }
                    }
                }
            };
            output.write(&buffer[..read]).map_err(fault)?;
            progressed = true;
            if read < requested {
                break;
            }
        }
        Ok(())
    }

    /// 按 line-discipline 单批预算写入 slave 输入；只在尚未写出任何字节时阻塞。
    fn write(&self, input: &mut dyn UserInput, nonblocking: bool) -> Result<(), DeviceError> {
        let master = &self.0;
        let mut buffer = [0u8; TERMINAL_READ_BYTES];
        let mut progressed = false;
        while input.remaining() != 0 {
            let requested = character_write_chunk(input.remaining(), true);
            input.copy(&mut buffer[..requested]).map_err(fault)?;
            let count = loop {
                match master.write(&buffer[..requested]) {
                    Ok(0) if progressed => return Ok(()),
                    Ok(0) if nonblocking => return Err(DeviceError::WouldBlock),
                    Ok(0) => {
                        if let Some(pipe) = master.prepare_write_to_block() {
                            waited(pipe.wait(PipeWaitCondition::Readable, None))?;
                        }
                    }
                    Ok(count) => break count,
                    Err(error) => return Err(terminal_error(error)),
                }
            };
            input.consume(count);
            progressed = true;
            if count < requested {
                break;
            }
        }
        Ok(())
    }

    fn poll(&self, events: i16) -> i16 {
        let master = &self.0;
        let input = if master.readable() {
            events & POLLIN
        } else {
            0
        };
        let output = if master.peer_hung_up() {
            POLLHUP
        } else if master.writable() {
            events & POLLOUT
        } else {
            0
        };
        input | output
    }

    fn wait_sources(&self, _events: i16) -> DeviceWaitSources {
        DeviceWaitSources::pipe(self.0.notification_pipe(), POLLIN | POLLOUT | POLLHUP)
    }

    fn readiness_generation(&self) -> u64 {
        self.0
            .notification_pipe()
            .readiness_generation(PipeDirection::Read)
    }

    fn prepare_wait(&self, _events: i16) -> Option<Arc<Pipe>> {
        self.0.prepare_to_block()
    }

    fn ioctl(&self, call: &IoctlCall<'_>) -> Result<isize, DeviceError> {
        ioctl::pty_master(&self.0, call)
    }
}
