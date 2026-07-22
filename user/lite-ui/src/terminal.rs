//! Event-driven client for the pure PTY/VT terminal helper.

use std::{
    io::{self, Read, Write},
    os::fd::{AsFd, BorrowedFd},
    os::unix::net::UnixStream,
    process::{ChildStdin, Command, Stdio},
    sync::mpsc::{self, Receiver},
    thread,
};

use linux_uapi::process::SessionChild;
use serde::Deserialize;
use serde_json::{Value, json};

const INPUT: u32 = 1;
const RESIZE: u32 = 2;
const ACK: u32 = 3;
const UPDATE: u32 = 4;
const EXIT: u32 = 5;
const MAX_MESSAGE: usize = 8 * 1024 * 1024;

enum Message {
    Update(Vec<u8>),
    Exit,
    Error(io::Error),
}

#[derive(Default)]
struct Modifiers {
    shift: bool,
    control: bool,
    alt: bool,
    caps: bool,
}

#[derive(Deserialize)]
struct KeyEvent {
    code: u32,
    value: i32,
}

/// One terminal helper process, control stream and readiness wakeup.
pub struct Terminal {
    _child: SessionChild,
    input: ChildStdin,
    messages: Receiver<Message>,
    wake: UnixStream,
    rows: Vec<String>,
    cursor: (u16, u16),
    modifiers: Modifiers,
}

impl Terminal {
    /// Spawns the checked helper with an explicit interactive shell argv.
    pub fn spawn() -> io::Result<Self> {
        let mut command = Command::new("/bin/terminal-session");
        command.args(["--", "/bin/sh"]);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        let mut child = SessionChild::spawn(&mut command)?;
        let input = child
            .take_stdin()
            .ok_or_else(|| io::Error::other("terminal helper stdin missing"))?;
        let output = child
            .take_stdout()
            .ok_or_else(|| io::Error::other("terminal helper stdout missing"))?;
        let (wake, mut notifier) = UnixStream::pair()?;
        wake.set_nonblocking(true)?;
        let (sender, messages) = mpsc::channel();
        thread::Builder::new()
            .name("terminal-protocol".to_owned())
            .spawn(move || {
                let mut output = output;
                loop {
                    let message = match read_message(&mut output) {
                        Ok(Some(message)) => message,
                        Ok(None) => Message::Exit,
                        Err(error) => Message::Error(error),
                    };
                    let stop = !matches!(message, Message::Update(_));
                    if sender.send(message).is_err() || notifier.write_all(&[1]).is_err() || stop {
                        return;
                    }
                }
            })?;
        Ok(Self {
            _child: child,
            input,
            messages,
            wake,
            rows: Vec::new(),
            cursor: (0, 0),
            modifiers: Modifiers::default(),
        })
    }

    /// Returns the reader used only to wake the LiteUI owner loop.
    pub fn as_fd(&self) -> BorrowedFd<'_> {
        self.wake.as_fd()
    }

    /// Applies all ready helper updates and returns the latest React screen value.
    pub fn drain(&mut self) -> io::Result<Option<Value>> {
        let mut wake = [0u8; 64];
        while self.wake.read(&mut wake).is_ok() {}
        while let Ok(message) = self.messages.try_recv() {
            match message {
                Message::Update(payload) => {
                    self.apply_update(&payload)?;
                    write_frame(&mut self.input, ACK, &[])?;
                }
                Message::Exit => return Ok(None),
                Message::Error(error) => return Err(error),
            }
        }
        Ok(Some(json!({
            "rows": self.rows,
            "cursor": {"column": self.cursor.0, "row": self.cursor.1}
        })))
    }

    /// Translates one routed Linux key event and writes its PTY byte sequence.
    pub fn input(&mut self, payload: &[u8]) -> io::Result<()> {
        let event: KeyEvent = serde_json::from_slice(payload)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        if let Some(bytes) = translate_key(&mut self.modifiers, event) {
            write_frame(&mut self.input, INPUT, &bytes)?;
        }
        Ok(())
    }

    /// Converts app pixels to a fixed terminal grid and sends a complete resize.
    pub fn resize(&mut self, width: u32, height: u32) -> io::Result<()> {
        let columns = (width / 8).max(1).min(u32::from(u16::MAX)) as u16;
        let rows = (height / 16).max(1).min(u32::from(u16::MAX)) as u16;
        let mut payload = [0u8; 8];
        payload[0..2].copy_from_slice(&columns.to_le_bytes());
        payload[2..4].copy_from_slice(&rows.to_le_bytes());
        payload[4..6].copy_from_slice(&(width.min(u32::from(u16::MAX)) as u16).to_le_bytes());
        payload[6..8].copy_from_slice(&(height.min(u32::from(u16::MAX)) as u16).to_le_bytes());
        write_frame(&mut self.input, RESIZE, &payload)
    }

    fn apply_update(&mut self, payload: &[u8]) -> io::Result<()> {
        if payload.len() < 12 {
            return Err(invalid("terminal update header truncated"));
        }
        let columns = read_u16(payload, 0)? as usize;
        let rows = read_u16(payload, 2)? as usize;
        self.cursor = (read_u16(payload, 4)?, read_u16(payload, 6)?);
        let dirty = read_u16(payload, 8)? as usize;
        if columns == 0 || rows == 0 || read_u16(payload, 10)? != 0 {
            return Err(invalid("terminal update geometry invalid"));
        }
        if self.rows.len() != rows {
            self.rows = vec![" ".repeat(columns); rows];
        }
        let mut offset = 12usize;
        for _ in 0..dirty {
            let row = read_u16(payload, offset)? as usize;
            if row >= rows || read_u16(payload, offset + 2)? != 0 {
                return Err(invalid("terminal dirty row invalid"));
            }
            offset += 4;
            let bytes = payload
                .get(
                    offset
                        ..offset
                            .checked_add(columns * 16)
                            .ok_or_else(|| invalid("terminal row overflow"))?,
                )
                .ok_or_else(|| invalid("terminal row truncated"))?;
            let mut text = String::with_capacity(columns);
            for cell in bytes.as_chunks::<16>().0 {
                let codepoint = u32::from_le_bytes(cell[0..4].try_into().expect("cell codepoint"));
                text.push(char::from_u32(codepoint).unwrap_or('\u{fffd}'));
            }
            self.rows[row] = text;
            offset += columns * 16;
        }
        if offset != payload.len() {
            return Err(invalid("terminal update has trailing bytes"));
        }
        Ok(())
    }
}

fn read_message(input: &mut impl Read) -> io::Result<Option<Message>> {
    let mut header = [0u8; 8];
    match input.read_exact(&mut header) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error),
    }
    let length = u32::from_le_bytes(header[..4].try_into().expect("terminal length")) as usize;
    let kind = u32::from_le_bytes(header[4..].try_into().expect("terminal kind"));
    if !(8..=MAX_MESSAGE).contains(&length) {
        return Err(invalid("terminal message length invalid"));
    }
    let mut payload = vec![0u8; length - 8];
    input.read_exact(&mut payload)?;
    match kind {
        UPDATE => Ok(Some(Message::Update(payload))),
        EXIT if payload.is_empty() => Ok(Some(Message::Exit)),
        _ => Err(invalid("terminal message kind invalid")),
    }
}

fn write_frame(output: &mut impl Write, kind: u32, payload: &[u8]) -> io::Result<()> {
    output.write_all(&((8 + payload.len()) as u32).to_le_bytes())?;
    output.write_all(&kind.to_le_bytes())?;
    output.write_all(payload)?;
    output.flush()
}

fn translate_key(state: &mut Modifiers, event: KeyEvent) -> Option<Vec<u8>> {
    let pressed = event.value != 0;
    match event.code {
        42 | 54 => state.shift = pressed,
        29 | 97 => state.control = pressed,
        56 | 100 => state.alt = pressed,
        58 if event.value == 1 => state.caps = !state.caps,
        _ => {}
    }
    if matches!(event.code, 29 | 42 | 54 | 56 | 58 | 97 | 100) || !pressed {
        return None;
    }
    let special: Option<&[u8]> = match event.code {
        1 => Some(b"\x1b"),
        14 => Some(b"\x7f"),
        15 => Some(if state.shift { b"\x1b[Z" } else { b"\t" }),
        28 => Some(b"\r"),
        102 => Some(b"\x1b[1~"),
        103 => Some(b"\x1b[A"),
        105 => Some(b"\x1b[D"),
        106 => Some(b"\x1b[C"),
        107 => Some(b"\x1b[4~"),
        108 => Some(b"\x1b[B"),
        109 => Some(b"\x1b[6~"),
        110 => Some(b"\x1b[2~"),
        111 => Some(b"\x1b[3~"),
        _ => None,
    };
    if let Some(bytes) = special {
        return Some(bytes.to_vec());
    }
    let mut character = plain_key(event.code as u16)?;
    if character.is_ascii_alphabetic() {
        if state.shift != state.caps {
            character.make_ascii_uppercase();
        }
    } else if state.shift {
        character = shifted_key(event.code as u16).unwrap_or(character);
    }
    if state.control {
        character = character
            .to_ascii_lowercase()
            .wrapping_sub(b'a')
            .wrapping_add(1);
    }
    let mut bytes = Vec::with_capacity(2);
    if state.alt {
        bytes.push(0x1b);
    }
    bytes.push(character);
    Some(bytes)
}

fn plain_key(code: u16) -> Option<u8> {
    Some(match code {
        2..=11 => *b"1234567890".get((code - 2) as usize)?,
        12 => b'-',
        13 => b'=',
        16..=27 => *b"qwertyuiop[]".get((code - 16) as usize)?,
        30..=41 => *b"asdfghjkl;'`".get((code - 30) as usize)?,
        43 => b'\\',
        44..=53 => *b"zxcvbnm,./".get((code - 44) as usize)?,
        57 => b' ',
        _ => return None,
    })
}

fn shifted_key(code: u16) -> Option<u8> {
    Some(match code {
        2..=13 => *b"!@#$%^&*()_+".get((code - 2) as usize)?,
        26 => b'{',
        27 => b'}',
        39 => b':',
        40 => b'"',
        41 => b'~',
        43 => b'|',
        51 => b'<',
        52 => b'>',
        53 => b'?',
        _ => return None,
    })
}

fn read_u16(bytes: &[u8], offset: usize) -> io::Result<u16> {
    bytes
        .get(offset..offset + 2)
        .and_then(|bytes| bytes.try_into().ok())
        .map(u16::from_le_bytes)
        .ok_or_else(|| invalid("terminal update truncated"))
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
