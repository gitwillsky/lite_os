use alloc::sync::Arc;
use spin::Mutex;

use super::{Console, FileSystemError};

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
    state: Mutex<TerminalState>,
}

impl Terminal {
    /// @description 用 Linux 风格 sane defaults 包装唯一 platform console。
    ///
    /// @param console UART-backed raw byte device。
    /// @return 可由所有 console OFD 共享的 TTY owner。
    pub(crate) fn new(console: Arc<dyn Console>) -> Arc<Self> {
        let mut termios = [0u8; KERNEL_TERMIOS_SIZE];
        termios[0..4].copy_from_slice(&0x500u32.to_ne_bytes());
        termios[4..8].copy_from_slice(&0x5u32.to_ne_bytes());
        termios[8..12].copy_from_slice(&0xbdu32.to_ne_bytes());
        termios[12..16].copy_from_slice(&0x8a3bu32.to_ne_bytes());
        termios[17..24].copy_from_slice(&[3, 28, 127, 21, 4, 0, 1]);
        let mut window_size = [0u8; 8];
        window_size[0..2].copy_from_slice(&24u16.to_ne_bytes());
        window_size[2..4].copy_from_slice(&80u16.to_ne_bytes());
        Arc::new(Self {
            console,
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
            }),
        })
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

    pub(crate) fn input_ready(&self) -> bool {
        let state = self.state.lock();
        state.input_len != 0 || state.eof_pending
    }

    pub(crate) fn wait_ready(&self) -> bool {
        self.input_ready() || self.console.input_ready()
    }

    pub(crate) fn write(&self, bytes: &[u8]) -> Result<usize, FileSystemError> {
        const OPOST: u32 = 0x1;
        const ONLCR: u32 = 0x4;
        if self.state.lock().output_flags() & (OPOST | ONLCR) != (OPOST | ONLCR) {
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
    /// @return 本批输入生成的 Linux signal bitset。
    /// @errors 底层 UART 读写失败或固定 cooked queue 已满时返回 `IoError`。
    pub(crate) fn drain_input(&self) -> Result<u64, FileSystemError> {
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
        let mut raw = [0u8; 128];
        loop {
            let count = self.console.read(&mut raw)?;
            if count == 0 {
                return Ok(signals);
            }
            let mut echo = [0u8; 512];
            let mut echo_len = 0;
            {
                let mut state = self.state.lock();
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
                        }
                    }
                    if local_flags & ECHO != 0 || byte == b'\n' && local_flags & ECHONL != 0 {
                        echo[echo_len] = byte;
                        echo_len += 1;
                    }
                }
            }
            if echo_len != 0 && self.write(&echo[..echo_len])? != echo_len {
                return Err(FileSystemError::IoError);
            }
        }
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

    pub(crate) fn window_size(&self) -> [u8; 8] {
        self.state.lock().window_size
    }

    pub(crate) fn set_window_size(&self, window_size: [u8; 8]) {
        self.state.lock().window_size = window_size;
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

    pub(crate) fn release_session(&self, session: usize) {
        let mut state = self.state.lock();
        if state.controlling_session == Some(session) {
            state.controlling_session = None;
            state.foreground_pgid = None;
        }
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
