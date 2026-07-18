use alloc::sync::Arc;
use spin::Mutex;

use super::{Console, DeviceKind, FileSystemError};

mod input_batch;
pub(crate) use input_batch::{TERMINAL_INPUT_BATCH_BYTES, character_write_chunk};
use input_batch::{TerminalInputBatch, terminal_input_chunk};

#[path = "terminal_flush.rs"]
mod terminal_flush;
pub(in crate::fs) use terminal_flush::clear_raw as clear_terminal_raw_input;

const KERNEL_TERMIOS_SIZE: usize = 36;
const TERMINAL_INPUT_CAPACITY: usize = 4096;
const TERMINAL_LINE_CAPACITY: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TerminalAccess {
    Input,
    Output,
    StateChange,
}

/// @description line discipline 对一次 read 的明确结果；Empty 与 canonical EOF 不混淆。
pub(crate) enum TerminalRead {
    Bytes(usize),
    Eof,
    Empty,
}

/// @description 当前 termios 对一次 terminal read 规定的完成条件。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TerminalReadMode {
    /// canonical line discipline 只发布完整行或 EOF。
    Canonical,
    /// noncanonical read 由 VMIN 与 VTIME 共同决定最小字节数和超时。
    Noncanonical {
        /// 一次 read 正常完成前所需的最小字节数，已限制到 caller capacity。
        minimum: usize,
        /// VTIME 表示的 decisecond timeout，转换为纳秒；零表示无超时。
        timeout_ns: u64,
    },
}

struct TerminalState {
    termios: [u8; KERNEL_TERMIOS_SIZE],
    window_size: [u8; 8],
    controlling_session: Option<usize>,
    foreground_pgid: Option<usize>,
    input: [u8; TERMINAL_INPUT_CAPACITY],
    input_head: usize,
    input_len: usize,
    line: [u8; TERMINAL_LINE_CAPACITY],
    line_len: usize,
    eof_pending: bool,
    input_generation: u64,
}

impl TerminalState {
    fn local_flags(&self) -> u32 {
        u32::from_ne_bytes(self.termios[12..16].try_into().unwrap())
    }

    fn input_flags(&self) -> u32 {
        u32::from_ne_bytes(self.termios[0..4].try_into().unwrap())
    }

    fn output_flags(&self) -> u32 {
        u32::from_ne_bytes(self.termios[4..8].try_into().unwrap())
    }

    fn push_input(&mut self, byte: u8) -> Result<(), ()> {
        if self.input_len == self.input.len() {
            return Err(());
        }
        let tail = (self.input_head + self.input_len) % self.input.len();
        self.input[tail] = byte;
        self.input_len += 1;
        Ok(())
    }

    fn commit_line(&mut self) -> Result<(), ()> {
        if self.input.len() - self.input_len < self.line_len {
            return Err(());
        }
        for index in 0..self.line_len {
            self.push_input(self.line[index])?;
        }
        self.line_len = 0;
        Ok(())
    }
}

/// @description console device、termios 与 session/foreground ownership 的唯一 TTY 对象。
pub(crate) struct Terminal {
    console: Arc<dyn Console>,
    // OWNER: 每个 Terminal 永久绑定创建它的实际 character device；`/dev/tty` 只是当前
    // Process handle 的别名。缺失该 identity 会让关闭所有 tty fd 后的 `/proc/pid/stat`
    // 无法继续准确投影 controlling terminal。
    device: DeviceKind,
    state: Mutex<TerminalState>,
}

impl Terminal {
    /// @description 用 Linux 风格 sane defaults 包装唯一 platform console。
    ///
    /// @param console raw byte device adapter。
    /// @param device `/dev/console` 或实际 `/dev/pts/N` identity；不得传入 `/dev/tty` 别名。
    /// @return 可由所有 console OFD 共享的 TTY owner。
    pub(crate) fn new(console: Arc<dyn Console>, device: DeviceKind) -> Result<Arc<Self>, ()> {
        assert_ne!(
            device,
            DeviceKind::Tty,
            "terminal cannot own /dev/tty alias"
        );
        let mut termios = [0u8; KERNEL_TERMIOS_SIZE];
        termios[0..4].copy_from_slice(&0x500u32.to_ne_bytes());
        termios[4..8].copy_from_slice(&0x5u32.to_ne_bytes());
        termios[8..12].copy_from_slice(&0xbdu32.to_ne_bytes());
        termios[12..16].copy_from_slice(&0x8a3bu32.to_ne_bytes());
        termios[17..24].copy_from_slice(&[3, 28, 127, 21, 4, 0, 1]);
        let mut window_size = [0u8; 8];
        window_size[0..2].copy_from_slice(&24u16.to_ne_bytes());
        window_size[2..4].copy_from_slice(&80u16.to_ne_bytes());
        Arc::try_new(Self {
            console,
            device,
            state: Mutex::new(TerminalState {
                termios,
                window_size,
                controlling_session: None,
                foreground_pgid: None,
                input: [0; TERMINAL_INPUT_CAPACITY],
                input_head: 0,
                input_len: 0,
                line: [0; TERMINAL_LINE_CAPACITY],
                line_len: 0,
                eof_pending: false,
                input_generation: crate::sync::next_readiness_generation(),
            }),
        })
        .map_err(|_| ())
    }

    /// @description 一次锁快照投影 Linux proc stat 的 controlling-terminal 字段。
    /// @param session 目标 Process 的 process-graph session ID。
    /// @return `(tty_nr, tpgid)`；无 controlling terminal 时固定为 `(0, -1)`。
    pub(crate) fn proc_identity(&self, session: usize) -> (u32, isize) {
        let state = self.state.lock();
        if state.controlling_session != Some(session) {
            return (0, -1);
        }
        let (major, minor) = self.device.numbers();
        // Linux new_encode_dev 保留低 8-bit minor，并把扩展 minor 放到 bit 20 以上。
        let device = (minor & 0xff) | (major << 8) | ((minor & !0xff) << 12);
        (
            device,
            state.foreground_pgid.map_or(-1, |pgid| pgid as isize),
        )
    }

    /// @description 从 line discipline 唯一 cooked queue 非阻塞读取。
    ///
    /// @param bytes kernel-owned 目标缓冲区。
    /// @return bytes、canonical EOF 或当前无完整输入。
    pub(crate) fn read(&self, bytes: &mut [u8]) -> TerminalRead {
        let mut state = self.state.lock();
        if state.input_len == 0 {
            if state.eof_pending {
                state.eof_pending = false;
                return TerminalRead::Eof;
            }
            return TerminalRead::Empty;
        }
        let count = bytes.len().min(state.input_len);
        for byte in bytes.iter_mut().take(count) {
            *byte = state.input[state.input_head];
            state.input_head = (state.input_head + 1) % state.input.len();
            state.input_len -= 1;
        }
        TerminalRead::Bytes(count)
    }

    /// @description 从唯一 termios owner 投影一次 read 的 canonical/VMIN/VTIME 语义。
    ///
    /// @param capacity 本次 userspace read 可接收的最大字节数。
    /// @return canonical 模式，或已按 capacity 收敛的 noncanonical 完成条件。
    pub(crate) fn read_mode(&self, capacity: usize) -> TerminalReadMode {
        const ICANON: u32 = 0x2;
        const VTIME: usize = 5;
        const VMIN: usize = 6;
        const CONTROL_OFFSET: usize = 17;
        const DECISECOND_NS: u64 = 100_000_000;

        let state = self.state.lock();
        if state.local_flags() & ICANON != 0 {
            return TerminalReadMode::Canonical;
        }
        TerminalReadMode::Noncanonical {
            minimum: usize::from(state.termios[CONTROL_OFFSET + VMIN]).min(capacity),
            timeout_ns: u64::from(state.termios[CONTROL_OFFSET + VTIME]) * DECISECOND_NS,
        }
    }

    pub(crate) fn input_ready(&self) -> bool {
        let state = self.state.lock();
        state.input_len != 0 || state.eof_pending
    }

    pub(crate) fn wait_ready(&self) -> bool {
        self.input_ready() || self.console.input_ready()
    }

    /// @description 返回 cooked input 最近一次变为可观察输入的全局 generation。
    ///
    /// @return 跨 I/O source 可比较的 generation。
    pub(crate) fn readiness_generation(&self) -> u64 {
        self.state.lock().input_generation
    }

    /// @description 在 Terminal→Console 唯一锁序下同步写出一批 terminal output。
    /// @param bytes kernel-owned output bytes。
    /// @return Console 已同步接收的 input byte 数。
    /// @errors Console adapter 写失败时返回 `IoError`。
    pub(crate) fn write(&self, bytes: &[u8]) -> Result<usize, FileSystemError> {
        let state = self.state.lock();
        let result = self.write_synchronous(bytes, state.output_flags());
        // TCSETSW/TCSETSF 取得同一 state lock 后，所有更早进入的同步 output 必已返回。
        drop(state);
        result
    }

    fn write_synchronous(&self, bytes: &[u8], output_flags: u32) -> Result<usize, FileSystemError> {
        const OPOST: u32 = 0x1;
        const ONLCR: u32 = 0x4;
        if output_flags & (OPOST | ONLCR) != (OPOST | ONLCR) {
            return self.console.write(bytes);
        }
        let mut consumed = 0;
        let mut output = [0u8; 256];
        while consumed < bytes.len() {
            let mut output_len = 0;
            let start = consumed;
            while consumed < bytes.len() && output_len < output.len() - 1 {
                let byte = bytes[consumed];
                if byte == b'\n' {
                    output[output_len] = b'\r';
                    output_len += 1;
                }
                output[output_len] = byte;
                output_len += 1;
                consumed += 1;
            }
            let written = self.console.write(&output[..output_len])?;
            if written != output_len {
                return Ok(start);
            }
        }
        Ok(bytes.len())
    }

    /// @description 在 deferred context 将 UART raw ring 唯一转换进 termios line discipline。
    ///
    /// @return 本批输入生成的 Linux signal bitset，以及 raw ring 是否仍有 backlog。
    /// @errors 底层 UART 读写失败或固定 cooked queue 已满时返回 `IoError`。
    pub(crate) fn drain_input(&self) -> Result<TerminalInputBatch, FileSystemError> {
        const IGNCR: u32 = 0x80;
        const ICRNL: u32 = 0x100;
        const INLCR: u32 = 0x40;
        const ISIG: u32 = 0x1;
        const ICANON: u32 = 0x2;
        const ECHO: u32 = 0x8;
        const ECHOE: u32 = 0x10;
        const ECHONL: u32 = 0x40;
        const ECHOCTL: u32 = 0x200;
        let mut signals = 0u64;
        let mut consumed = 0usize;
        let mut raw = [0u8; 128];
        while consumed < TERMINAL_INPUT_BATCH_BYTES {
            let mut echo = [0u8; 512];
            {
                let mut state = self.state.lock();
                let capacity = terminal_input_chunk(consumed, raw.len());
                let count = self.console.read(&mut raw[..capacity])?;
                if count == 0 {
                    return Ok(TerminalInputBatch {
                        signals,
                        backlog: false,
                    });
                }
                assert!(
                    count <= raw.len(),
                    "console returned more bytes than requested"
                );
                consumed += count;
                let mut echo_len = 0;
                for mut byte in raw[..count].iter().copied() {
                    let input_flags = state.input_flags();
                    let local_flags = state.local_flags();
                    if byte == b'\r' {
                        if input_flags & IGNCR != 0 {
                            continue;
                        }
                        if input_flags & ICRNL != 0 {
                            byte = b'\n';
                        }
                    } else if byte == b'\n' && input_flags & INLCR != 0 {
                        byte = b'\r';
                    }
                    let control = |index: usize| state.termios[17 + index];
                    let signal = if local_flags & ISIG != 0 && byte == control(0) {
                        Some(2usize)
                    } else if local_flags & ISIG != 0 && byte == control(1) {
                        Some(3usize)
                    } else if local_flags & ISIG != 0 && byte == control(10) {
                        Some(20usize)
                    } else {
                        None
                    };
                    if let Some(signal) = signal {
                        signals |= 1u64 << (signal - 1);
                        state.line_len = 0;
                        if local_flags & ECHO != 0 {
                            if local_flags & ECHOCTL != 0 && byte < 0x20 {
                                echo[echo_len] = b'^';
                                echo[echo_len + 1] = byte + b'@';
                                echo_len += 2;
                            }
                            echo[echo_len] = b'\n';
                            echo_len += 1;
                        }
                        continue;
                    }
                    if local_flags & ICANON == 0 {
                        state
                            .push_input(byte)
                            .map_err(|()| FileSystemError::IoError)?;
                        state.input_generation = crate::sync::next_readiness_generation();
                    } else if byte == control(2) {
                        if state.line_len != 0 {
                            state.line_len -= 1;
                            if local_flags & ECHO != 0 {
                                if local_flags & ECHOE != 0 {
                                    echo[echo_len..echo_len + 3].copy_from_slice(b"\x08 \x08");
                                    echo_len += 3;
                                } else {
                                    echo[echo_len] = byte;
                                    echo_len += 1;
                                }
                            }
                        }
                        continue;
                    } else if byte == control(3) {
                        state.line_len = 0;
                        continue;
                    } else if byte == control(4) {
                        if state.line_len == 0 {
                            state.eof_pending = true;
                        } else {
                            state.commit_line().map_err(|()| FileSystemError::IoError)?;
                        }
                        state.input_generation = crate::sync::next_readiness_generation();
                        continue;
                    } else {
                        if state.line_len == state.line.len() {
                            return Err(FileSystemError::IoError);
                        }
                        let line_len = state.line_len;
                        state.line[line_len] = byte;
                        state.line_len += 1;
                        if byte == b'\n' {
                            state.commit_line().map_err(|()| FileSystemError::IoError)?;
                            state.input_generation = crate::sync::next_readiness_generation();
                        }
                    }
                    if local_flags & ECHO != 0 || byte == b'\n' && local_flags & ECHONL != 0 {
                        echo[echo_len] = byte;
                        echo_len += 1;
                    }
                }
                if echo_len != 0
                    && self.write_synchronous(&echo[..echo_len], state.output_flags())? != echo_len
                {
                    return Err(FileSystemError::IoError);
                }
            }
        }
        Ok(TerminalInputBatch {
            signals,
            backlog: self.console.input_ready(),
        })
    }

    /// @description 根据 controlling session、foreground group 与 TOSTOP 决定后台访问 signal。
    ///
    /// @param session caller 的 session ID。
    /// @param process_group caller 的 process group ID。
    /// @param access 输入、输出或 TTY 状态修改。
    /// @return 非 controlling/background 豁免返回 `None`，否则返回 SIGTTIN/SIGTTOU。
    pub(crate) fn background_signal(
        &self,
        session: usize,
        process_group: usize,
        access: TerminalAccess,
    ) -> Option<usize> {
        const TOSTOP: u32 = 0x100;
        let state = self.state.lock();
        if state.controlling_session != Some(session)
            || state
                .foreground_pgid
                .is_none_or(|group| group == process_group)
        {
            return None;
        }
        match access {
            TerminalAccess::Input => Some(21),
            TerminalAccess::Output if state.local_flags() & TOSTOP != 0 => Some(22),
            TerminalAccess::Output => None,
            TerminalAccess::StateChange => Some(22),
        }
    }

    pub(crate) fn signal_target_group(&self) -> Option<usize> {
        self.state.lock().foreground_pgid
    }

    pub(crate) fn termios(&self) -> [u8; KERNEL_TERMIOS_SIZE] {
        self.state.lock().termios
    }

    pub(crate) fn set_termios(&self, termios: [u8; KERNEL_TERMIOS_SIZE]) {
        self.state.lock().termios = termios;
    }

    /// @description 在当前同步 Console output contract 的 drain point 应用 termios。
    /// @param termios 完整 Linux kernel termios layout。
    /// @return 无返回值；Console::write 返回后 Terminal 不保留待发送 output，因此无需等待队列。
    pub(crate) fn set_termios_after_output(&self, termios: [u8; KERNEL_TERMIOS_SIZE]) {
        self.state.lock().termios = termios;
    }

    /// @description 在同步 output drain point 丢弃 raw/cooked input 后应用 termios。
    /// @param termios 完整 Linux kernel termios layout。
    /// @return 无返回值；termios 已应用且所有调用前 pending input 已不可见。
    pub(crate) fn flush_input_and_set_termios(&self, termios: [u8; KERNEL_TERMIOS_SIZE]) {
        // 与 drain_input 使用同一 Terminal→Console lock order，使 raw dequeue、cooked publication
        // 与本次 flush 可以线性化；缺失该顺序会让已经从 raw ring 取出的旧字节越过 TCSETSF。
        let mut state = self.state.lock();
        let raw = self.console.discard_input();
        let TerminalState {
            input_head,
            input_len,
            line_len,
            eof_pending,
            ..
        } = &mut *state;
        let cooked = terminal_flush::clear_pending(input_head, input_len, line_len, eof_pending);
        state.termios = termios;
        if raw != 0 || cooked {
            state.input_generation = crate::sync::next_readiness_generation();
        }
    }

    pub(crate) fn window_size(&self) -> [u8; 8] {
        self.state.lock().window_size
    }

    /// @description 原子替换窗口尺寸，并返回需要接收 `SIGWINCH` 的 foreground group。
    /// @param window_size Linux `struct winsize` 的 8-byte native layout。
    /// @return 尺寸变化且存在 foreground group 时返回 PGID；未变化或无 group 返回 `None`。
    pub(crate) fn set_window_size(&self, window_size: [u8; 8]) -> Option<usize> {
        let mut state = self.state.lock();
        if state.window_size == window_size {
            return None;
        }
        state.window_size = window_size;
        state.foreground_pgid
    }

    pub(crate) fn controlling_session(&self) -> Option<usize> {
        self.state.lock().controlling_session
    }

    pub(crate) fn claim_session(&self, session: usize, foreground_pgid: usize) -> Result<(), ()> {
        let mut state = self.state.lock();
        if state
            .controlling_session
            .is_some_and(|owner| owner != session)
        {
            return Err(());
        }
        state.controlling_session = Some(session);
        state.foreground_pgid = Some(foreground_pgid);
        Ok(())
    }

    /// @description 原子释放 controlling session 并取走退出时应接收 SIGHUP 的 foreground PGID。
    ///
    /// @param session 正在退出的 session leader ID。
    /// @return session 匹配时返回原 foreground PGID，否则返回 None。
    pub(crate) fn release_session(&self, session: usize) -> Option<usize> {
        let mut state = self.state.lock();
        if state.controlling_session == Some(session) {
            state.controlling_session = None;
            return state.foreground_pgid.take();
        }
        None
    }

    /// @description 原子执行 PTY vhangup，清除 controlling session 与 foreground owner。
    /// @return 原 foreground PGID；task owner 用它投递 SIGHUP/SIGCONT。
    pub(crate) fn hangup(&self) -> Option<usize> {
        let mut state = self.state.lock();
        state.controlling_session = None;
        state.foreground_pgid.take()
    }

    pub(crate) fn foreground_pgid(&self, session: usize) -> Result<usize, ()> {
        let state = self.state.lock();
        if state.controlling_session != Some(session) {
            return Err(());
        }
        state.foreground_pgid.ok_or(())
    }

    pub(crate) fn set_foreground_pgid(&self, session: usize, pgid: usize) -> Result<(), ()> {
        let mut state = self.state.lock();
        if state.controlling_session != Some(session) {
            return Err(());
        }
        state.foreground_pgid = Some(pgid);
        Ok(())
    }
}
