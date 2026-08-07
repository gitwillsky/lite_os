//! Event-driven client for the pure PTY/VT terminal helper.

mod control;
mod screen;

use std::{
    io::{self, Read, Write},
    os::fd::{AsFd, BorrowedFd},
    os::unix::net::UnixStream,
    process::ChildStdin,
    sync::mpsc::{self, Receiver},
    thread,
};

use linux_uapi::process::{SessionChild, SessionCommand, SessionIo};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::keymap::{Modifiers, character};
use screen::ScreenState;
#[cfg(test)]
use screen::{ATTR_BOLD, ATTR_HIDDEN, ATTR_INVERSE, Run, WIDE_CONTINUATION, runs};

const INPUT: u32 = 1;
const RESIZE: u32 = 2;
const ACK: u32 = 3;
const UPDATE: u32 = 4;
const EXIT: u32 = 5;
const SELECT: u32 = 6;
const SCROLL: u32 = 7;
const APPLICATION_CURSOR_KEYS: u32 = 1;
const MAX_MESSAGE: usize = 8 * 1024 * 1024;

enum Message {
    Update(Vec<u8>),
    Exit,
    Error(io::Error),
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
    screen: ScreenState,
    modifiers: Modifiers,
}

impl Terminal {
    /// Spawns the checked helper with an explicit interactive shell argv.
    pub fn spawn() -> io::Result<Self> {
        let mut child = SessionChild::spawn(SessionCommand::new(
            "/bin/terminal-session",
            vec!["--".into(), "/bin/sh".into()],
            SessionIo::Piped,
        ))?;
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
        let (cursor_shape, cursor_blinking) =
            cursor_appearance(self.screen.cursor_style).expect("validated cursor style");
        Ok(Some(json!({
            "columns": self.screen.columns,
            "rows": self.screen.rows,
            "cursor": {
                "column": self.screen.cursor.0,
                "row": self.screen.cursor.1,
                "visible": self.screen.cursor.0 != u16::MAX && self.screen.cursor.1 != u16::MAX,
                "shape": cursor_shape,
                "blinking": cursor_blinking,
            },
            "foreground": self.screen.foreground,
            "background": self.screen.background,
            "selection": self.screen.selection,
            "scrollOffset": self.screen.scroll_offset,
            "historyRows": self.screen.history_rows,
        })))
    }

    /// Translates one routed Linux key event and writes its PTY byte sequence.
    pub fn input(&mut self, payload: &[u8]) -> io::Result<()> {
        let event: KeyEvent = serde_json::from_slice(payload)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        if let Some(bytes) = translate_key(
            &mut self.modifiers,
            event,
            self.screen.application_cursor_keys,
        ) {
            write_frame(&mut self.input, INPUT, &bytes)?;
        }
        Ok(())
    }
}

fn cursor_appearance(style: u16) -> Option<(&'static str, bool)> {
    match style {
        1 => Some(("block", true)),
        2 => Some(("block", false)),
        3 => Some(("underline", true)),
        4 => Some(("underline", false)),
        5 => Some(("bar", true)),
        6 => Some(("bar", false)),
        _ => None,
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

fn translate_key(
    state: &mut Modifiers,
    event: KeyEvent,
    application_cursor_keys: bool,
) -> Option<Vec<u8>> {
    let pressed = event.value != 0;
    // 修饰键折叠交给共享 keymap，避免与 UI 文本输入各持一份键码表。
    if state.apply(event.code, event.value) || !pressed {
        return None;
    }
    let special: Option<&[u8]> = match event.code {
        1 => Some(b"\x1b"),
        14 => Some(b"\x7f"),
        15 => Some(if state.shift { b"\x1b[Z" } else { b"\t" }),
        28 => Some(b"\r"),
        102 => Some(if application_cursor_keys {
            b"\x1bOH"
        } else {
            b"\x1b[1~"
        }),
        103 => Some(if application_cursor_keys {
            b"\x1bOA"
        } else {
            b"\x1b[A"
        }),
        104 => Some(b"\x1b[5~"),
        105 => Some(if application_cursor_keys {
            b"\x1bOD"
        } else {
            b"\x1b[D"
        }),
        106 => Some(if application_cursor_keys {
            b"\x1bOC"
        } else {
            b"\x1b[C"
        }),
        107 => Some(if application_cursor_keys {
            b"\x1bOF"
        } else {
            b"\x1b[4~"
        }),
        108 => Some(if application_cursor_keys {
            b"\x1bOB"
        } else {
            b"\x1b[B"
        }),
        109 => Some(b"\x1b[6~"),
        110 => Some(b"\x1b[2~"),
        111 => Some(b"\x1b[3~"),
        _ => None,
    };
    if let Some(bytes) = special {
        return Some(bytes.to_vec());
    }
    // 共享 keymap 出字符（含 shift/caps），终端再叠加 control/alt 的 PTY 转义。
    let mut character = character(event.code as u16, *state)? as u8;
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

    fn cell(codepoint: char, fg: u32, bg: u32, attributes: u16, metadata: u16) -> [u8; 16] {
        let mut bytes = [0u8; 16];
        bytes[0..4].copy_from_slice(&(codepoint as u32).to_le_bytes());
        bytes[4..8].copy_from_slice(&fg.to_le_bytes());
        bytes[8..12].copy_from_slice(&bg.to_le_bytes());
        bytes[12..14].copy_from_slice(&attributes.to_le_bytes());
        bytes[14..16].copy_from_slice(&metadata.to_le_bytes());
        bytes
    }

    fn run(text: &str, columns: usize, fg: u32, bg: u32, bold: bool) -> Run {
        Run {
            text: text.to_owned(),
            columns,
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
        payload.extend_from_slice(&2u16.to_le_bytes()); // steady block cursor
        payload.extend_from_slice(&FG.to_le_bytes()); // default foreground
        payload.extend_from_slice(&BG.to_le_bytes()); // default background
        payload.extend_from_slice(&0u32.to_le_bytes()); // normal cursor-key mode
        payload.extend_from_slice(&[0xff; 8]); // no selection
        payload.extend_from_slice(&0u32.to_le_bytes()); // selection text length
        payload.extend_from_slice(&0u16.to_le_bytes()); // live viewport
        payload.extend_from_slice(&0u16.to_le_bytes()); // empty scrollback
        payload.extend_from_slice(&1u16.to_le_bytes()); // dirty row index
        payload.extend_from_slice(&0u16.to_le_bytes());
        for character in ['a', 'b', 'c'] {
            payload.extend_from_slice(&cell(character, FG, BG, 0, 0));
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
        assert_eq!(state.cursor_style, 2);
        assert_eq!(state.foreground, FG);
        assert_eq!(state.background, BG);
        assert!(!state.application_cursor_keys);
        assert_eq!(
            state.rows,
            vec![Vec::new(), vec![run("abc", 3, FG, BG, false)]]
        );
    }

    #[test]
    fn update_projects_application_cursor_mode_into_navigation_encoding() {
        let mut payload = update_payload();
        payload[20..24].copy_from_slice(&APPLICATION_CURSOR_KEYS.to_le_bytes());
        let mut screen = ScreenState::default();
        screen.apply_update(&payload).expect("valid input mode");
        assert!(screen.application_cursor_keys);
        for (code, expected) in [
            (102, b"\x1bOH".as_slice()),
            (103, b"\x1bOA".as_slice()),
            (105, b"\x1bOD".as_slice()),
            (106, b"\x1bOC".as_slice()),
            (107, b"\x1bOF".as_slice()),
            (108, b"\x1bOB".as_slice()),
        ] {
            assert_eq!(
                translate_key(
                    &mut Modifiers::default(),
                    KeyEvent { code, value: 1 },
                    screen.application_cursor_keys,
                )
                .as_deref(),
                Some(expected),
            );
        }
    }

    #[test]
    fn normal_cursor_mode_keeps_csi_navigation_encoding() {
        for (code, expected) in [
            (103, b"\x1b[A".as_slice()),
            (105, b"\x1b[D".as_slice()),
            (106, b"\x1b[C".as_slice()),
            (108, b"\x1b[B".as_slice()),
        ] {
            assert_eq!(
                translate_key(
                    &mut Modifiers::default(),
                    KeyEvent { code, value: 1 },
                    false,
                )
                .as_deref(),
                Some(expected),
            );
        }
    }

    #[test]
    fn update_decodes_all_decscusr_cursor_styles() {
        for (style, expected) in [
            (1u16, ("block", true)),
            (2, ("block", false)),
            (3, ("underline", true)),
            (4, ("underline", false)),
            (5, ("bar", true)),
            (6, ("bar", false)),
        ] {
            let mut payload = update_payload();
            payload[10..12].copy_from_slice(&style.to_le_bytes());
            let mut state = ScreenState::default();
            state.apply_update(&payload).expect("valid cursor style");
            assert_eq!(cursor_appearance(state.cursor_style), Some(expected));
        }
    }

    #[test]
    fn update_rejects_unknown_cursor_style() {
        let mut payload = update_payload();
        payload[10..12].copy_from_slice(&7u16.to_le_bytes());
        assert!(ScreenState::default().apply_update(&payload).is_err());
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
            cell('a', FG, BG, 0, 0),
            cell('b', 0x00ff_0000, BG, 0, 0),
            cell('c', 0x00ff_0000, BG, 0, 0),
        ]
        .concat();
        assert_eq!(
            runs(&bytes, BG),
            vec![
                run("a", 1, FG, BG, false),
                run("bc", 2, 0x00ff_0000, BG, false)
            ]
        );
    }

    #[test]
    fn runs_resolve_bold_inverse_and_hidden_attributes() {
        // The hidden cell sits mid-row: as a trailing default-background space
        // it would be trimmed instead of asserting the space substitution.
        let bytes = [
            cell('a', FG, BG, ATTR_BOLD, 0),
            cell('c', FG, BG, ATTR_HIDDEN, 0),
            cell('b', FG, BG, ATTR_INVERSE, 0),
        ]
        .concat();
        assert_eq!(
            runs(&bytes, BG),
            vec![
                run("a", 1, FG, BG, true),
                run(" ", 1, FG, BG, false),
                run("b", 1, BG, FG, false),
            ]
        );
    }

    #[test]
    fn runs_trim_only_whole_trailing_default_background_space_runs() {
        // One merged run keeps its tail: `ab  ` on the default background still
        // paints the container color, so dropping spaces inside a run is never
        // required for correctness.
        let merged = [
            cell('a', FG, BG, 0, 0),
            cell('b', FG, BG, 0, 0),
            cell(' ', FG, BG, 0, 0),
        ]
        .concat();
        assert_eq!(runs(&merged, BG), vec![run("ab ", 3, FG, BG, false)]);
        // A style boundary before the tail makes the tail its own run, which
        // trims away; a non-default background tail always stays visible.
        let split = [cell('a', FG, 0x0000_00ff, 0, 0), cell(' ', FG, BG, 0, 0)].concat();
        assert_eq!(runs(&split, BG), vec![run("a", 1, FG, 0x0000_00ff, false)]);
        let visible = [cell('a', FG, BG, 0, 0), cell(' ', FG, 0x0000_00ff, 0, 0)].concat();
        assert_eq!(
            runs(&visible, BG),
            vec![
                run("a", 1, FG, BG, false),
                run(" ", 1, FG, 0x0000_00ff, false)
            ]
        );
    }

    #[test]
    fn wide_continuation_counts_as_a_column_without_emitting_text() {
        let bytes = [
            cell('中', FG, BG, 0, 0),
            cell('\0', FG, BG, 0, WIDE_CONTINUATION),
            cell('x', FG, BG, 0, 0),
        ]
        .concat();

        assert_eq!(runs(&bytes, BG), vec![run("中x", 3, FG, BG, false)]);
    }
}
