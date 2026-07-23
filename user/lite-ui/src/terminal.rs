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
use serde::{Deserialize, Serialize};
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

// Cell attribute bits mirror `terminal-session/src/model.rs`; underline, dim
// and blink have no raster in the fixed-cell atlas and are rendered as normal
// text.
const ATTR_BOLD: u16 = 1 << 0;
const ATTR_INVERSE: u16 = 1 << 3;
const ATTR_HIDDEN: u16 = 1 << 4;

/// One maximal same-style cell run inside one screen row.
#[derive(Clone, Debug, PartialEq, Serialize)]
struct Run {
    text: String,
    fg: u32,
    bg: u32,
    bold: bool,
}

/// Latest decoded helper screen: per-row style runs, the `(column, row)`
/// cursor and the current default colors.
#[derive(Default)]
struct ScreenState {
    rows: Vec<Vec<Run>>,
    cursor: (u16, u16),
    foreground: u32,
    background: u32,
}

impl ScreenState {
    fn apply_update(&mut self, payload: &[u8]) -> io::Result<()> {
        if payload.len() < 20 {
            return Err(invalid("terminal update header truncated"));
        }
        let columns = read_u16(payload, 0)? as usize;
        let rows = read_u16(payload, 2)? as usize;
        // Header order pins `columns, rows, cursor_column, cursor_row`; the
        // helper writer in terminal-session emits the same sequence.
        self.cursor = (read_u16(payload, 4)?, read_u16(payload, 6)?);
        let dirty = read_u16(payload, 8)? as usize;
        if columns == 0 || rows == 0 || read_u16(payload, 10)? != 0 {
            return Err(invalid("terminal update geometry invalid"));
        }
        self.foreground = read_u32(payload, 12)?;
        self.background = read_u32(payload, 16)?;
        if self.rows.len() != rows {
            self.rows = vec![Vec::new(); rows];
        }
        let mut offset = 20usize;
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
            self.rows[row] = runs(bytes, self.background);
            offset += columns * 16;
        }
        if offset != payload.len() {
            return Err(invalid("terminal update has trailing bytes"));
        }
        Ok(())
    }
}

/// Collapses one full-width cell row into same-style runs and drops the
/// trailing invisible tail: whole runs of spaces on the default background
/// paint exactly the container color, so sending them would only grow the
/// bridge payload.
fn runs(bytes: &[u8], default_background: u32) -> Vec<Run> {
    let mut runs: Vec<Run> = Vec::new();
    for cell in bytes.as_chunks::<16>().0 {
        let mut codepoint = u32::from_le_bytes(cell[0..4].try_into().expect("cell codepoint"));
        let mut fg = u32::from_le_bytes(cell[4..8].try_into().expect("cell foreground"));
        let mut bg = u32::from_le_bytes(cell[8..12].try_into().expect("cell background"));
        let attributes = u16::from_le_bytes(cell[12..14].try_into().expect("cell attributes"));
        if attributes & ATTR_HIDDEN != 0 {
            codepoint = b' ' as u32;
        }
        if attributes & ATTR_INVERSE != 0 {
            std::mem::swap(&mut fg, &mut bg);
        }
        let bold = attributes & ATTR_BOLD != 0;
        let character = char::from_u32(codepoint).unwrap_or('\u{fffd}');
        match runs.last_mut() {
            Some(run) if run.fg == fg && run.bg == bg && run.bold == bold => {
                run.text.push(character)
            }
            _ => runs.push(Run {
                text: character.into(),
                fg,
                bg,
                bold,
            }),
        }
    }
    while runs
        .last()
        .is_some_and(|run| run.bg == default_background && run.text.chars().all(|c| c == ' '))
    {
        runs.pop();
    }
    runs
}

/// One terminal helper process, control stream and readiness wakeup.
pub struct Terminal {
    _child: SessionChild,
    input: ChildStdin,
    messages: Receiver<Message>,
    wake: UnixStream,
    screen: ScreenState,
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
            screen: ScreenState::default(),
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
                    self.screen.apply_update(&payload)?;
                    write_frame(&mut self.input, ACK, &[])?;
                }
                Message::Exit => return Ok(None),
                Message::Error(error) => return Err(error),
            }
        }
        Ok(Some(json!({
            "rows": self.screen.rows,
            "cursor": {"column": self.screen.cursor.0, "row": self.screen.cursor.1},
            "foreground": self.screen.foreground,
            "background": self.screen.background,
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

fn read_u32(bytes: &[u8], offset: usize) -> io::Result<u32> {
    bytes
        .get(offset..offset + 4)
        .and_then(|bytes| bytes.try_into().ok())
        .map(u32::from_le_bytes)
        .ok_or_else(|| invalid("terminal update truncated"))
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FG: u32 = 0x00cb_d5e1;
    const BG: u32 = 0x0010_1418;

    fn cell(codepoint: char, fg: u32, bg: u32, attributes: u16) -> [u8; 16] {
        let mut bytes = [0u8; 16];
        bytes[0..4].copy_from_slice(&(codepoint as u32).to_le_bytes());
        bytes[4..8].copy_from_slice(&fg.to_le_bytes());
        bytes[8..12].copy_from_slice(&bg.to_le_bytes());
        bytes[12..14].copy_from_slice(&attributes.to_le_bytes());
        bytes
    }

    fn run(text: &str, fg: u32, bg: u32, bold: bool) -> Run {
        Run {
            text: text.to_owned(),
            fg,
            bg,
            bold,
        }
    }

    /// Builds one minimal UPDATE payload: 3x2 grid, cursor at column 2 row 1,
    /// default colors, and one dirty row carrying `abc`.
    fn update_payload() -> Vec<u8> {
        let mut payload = Vec::new();
        payload.extend_from_slice(&3u16.to_le_bytes()); // columns
        payload.extend_from_slice(&2u16.to_le_bytes()); // rows
        payload.extend_from_slice(&2u16.to_le_bytes()); // cursor column
        payload.extend_from_slice(&1u16.to_le_bytes()); // cursor row
        payload.extend_from_slice(&1u16.to_le_bytes()); // dirty row count
        payload.extend_from_slice(&0u16.to_le_bytes());
        payload.extend_from_slice(&FG.to_le_bytes()); // default foreground
        payload.extend_from_slice(&BG.to_le_bytes()); // default background
        payload.extend_from_slice(&1u16.to_le_bytes()); // dirty row index
        payload.extend_from_slice(&0u16.to_le_bytes());
        for character in ['a', 'b', 'c'] {
            payload.extend_from_slice(&cell(character, FG, BG, 0));
        }
        payload
    }

    #[test]
    fn update_decodes_cursor_as_column_then_row() {
        let mut state = ScreenState::default();
        state.apply_update(&update_payload()).expect("valid update");
        // Distinct column/row values catch a swapped decode: (1, 2) would pass
        // a shape check but mirror the cursor across the grid diagonal.
        assert_eq!(state.cursor, (2, 1));
        assert_eq!(state.foreground, FG);
        assert_eq!(state.background, BG);
        assert_eq!(
            state.rows,
            vec![Vec::new(), vec![run("abc", FG, BG, false)]]
        );
    }

    #[test]
    fn update_rejects_truncated_row() {
        let mut payload = update_payload();
        payload.truncate(payload.len() - 8);
        let mut state = ScreenState::default();
        assert!(state.apply_update(&payload).is_err());
    }

    #[test]
    fn runs_split_on_style_change_and_merge_equal_neighbors() {
        let bytes = [
            cell('a', FG, BG, 0),
            cell('b', 0x00ff_0000, BG, 0),
            cell('c', 0x00ff_0000, BG, 0),
        ]
        .concat();
        assert_eq!(
            runs(&bytes, BG),
            vec![run("a", FG, BG, false), run("bc", 0x00ff_0000, BG, false)]
        );
    }

    #[test]
    fn runs_resolve_bold_inverse_and_hidden_attributes() {
        // The hidden cell sits mid-row: as a trailing default-background space
        // it would be trimmed instead of asserting the space substitution.
        let bytes = [
            cell('a', FG, BG, ATTR_BOLD),
            cell('c', FG, BG, ATTR_HIDDEN),
            cell('b', FG, BG, ATTR_INVERSE),
        ]
        .concat();
        assert_eq!(
            runs(&bytes, BG),
            vec![
                run("a", FG, BG, true),
                run(" ", FG, BG, false),
                run("b", BG, FG, false),
            ]
        );
    }

    #[test]
    fn runs_trim_only_whole_trailing_default_background_space_runs() {
        // One merged run keeps its tail: `ab  ` on the default background still
        // paints the container color, so dropping spaces inside a run is never
        // required for correctness.
        let merged = [
            cell('a', FG, BG, 0),
            cell('b', FG, BG, 0),
            cell(' ', FG, BG, 0),
        ]
        .concat();
        assert_eq!(runs(&merged, BG), vec![run("ab ", FG, BG, false)]);
        // A style boundary before the tail makes the tail its own run, which
        // trims away; a non-default background tail always stays visible.
        let split = [cell('a', FG, 0x0000_00ff, 0), cell(' ', FG, BG, 0)].concat();
        assert_eq!(runs(&split, BG), vec![run("a", FG, 0x0000_00ff, false)]);
        let visible = [cell('a', FG, BG, 0), cell(' ', FG, 0x0000_00ff, 0)].concat();
        assert_eq!(
            runs(&visible, BG),
            vec![run("a", FG, BG, false), run(" ", FG, 0x0000_00ff, false)]
        );
    }
}
