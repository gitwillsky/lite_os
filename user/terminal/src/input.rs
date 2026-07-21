//! 键盘翻译：桌面转发的 `INPUT_KEY`（原 evdev `EV_KEY` 的 `{code, value}`）经
//! keymap 译为 ANSI 转义序列写入 PTY。翻译逻辑搬自 console-session 的
//! `reactor/input.rs`；evdev 设备发现与 `InputEvent` 读取已随桌面输入转发退役。

use linux_uapi::pty::PtySession;

use crate::model::Model;

const INPUT_CAPACITY: usize = 4 * 1024;
pub const MAX_KEY_BYTES: usize = 8;
// 当前最长 reply 是 CPR（14 bytes），最短触发序列是两字节 ESC Z。按 8 倍预留后，
// session 可以在读取 PTY 前证明本批全部 device reply 都能原子进入固定 ring。
pub const PTY_REPLY_EXPANSION: usize = 8;

#[derive(Default)]
pub struct KeyboardState {
    shift: bool,
    control: bool,
    alt: bool,
    caps_lock: bool,
    num_lock: bool,
}

/// 桌面在 evdev `SYN_DROPPED` 后发送 `INPUT_SYNC_RESET`：此前 modifier snapshot
/// 不再可信，清零可避免 Shift/Ctrl 永久粘住（对齐原 input.rs 对 SYN_DROPPED 的处理）。
pub fn reset_modifiers(state: &mut KeyboardState) {
    *state = KeyboardState::default();
}

/// 处理一条 `INPUT_KEY{code, value}`（语义等同原 evdev `EV_KEY` 事件）。
pub fn handle_key(
    input: &mut InputQueue,
    state: &mut KeyboardState,
    code: u32,
    value: i32,
    model: &Model,
) {
    let Ok(code) = u16::try_from(code) else {
        return;
    };
    let pressed = value != 0;
    match code {
        42 | 54 => {
            state.shift = pressed;
            return;
        }
        29 | 97 => {
            state.control = pressed;
            return;
        }
        56 | 100 => {
            state.alt = pressed;
            return;
        }
        58 => {
            if value == 1 {
                state.caps_lock = !state.caps_lock;
            }
            return;
        }
        69 => {
            if value == 1 {
                state.num_lock = !state.num_lock;
            }
            return;
        }
        _ => {}
    }
    if !pressed || value == 2 && !model.autorepeat() {
        return;
    }
    if let Some(sequence) = keypad_key(
        code,
        model.application_keypad(),
        state.num_lock != state.shift,
        model.newline_mode(),
    ) {
        input.push(sequence);
        return;
    }
    let application_cursor = model.application_cursor_keys();
    let sequence: &[u8] = match code {
        1 => b"\x1b",
        14 => b"\x7f",
        15 if state.shift => b"\x1b[Z",
        15 => b"\t",
        28 if model.newline_mode() => b"\r\n",
        28 => b"\r",
        59 if state.shift => b"\x1b[25~",
        60 if state.shift => b"\x1b[26~",
        61 if state.shift => b"\x1b[28~",
        62 if state.shift => b"\x1b[29~",
        63 if state.shift => b"\x1b[31~",
        64 if state.shift => b"\x1b[32~",
        65 if state.shift => b"\x1b[33~",
        66 if state.shift => b"\x1b[34~",
        59 => b"\x1b[[A",
        60 => b"\x1b[[B",
        61 => b"\x1b[[C",
        62 => b"\x1b[[D",
        63 => b"\x1b[[E",
        64 => b"\x1b[17~",
        65 => b"\x1b[18~",
        66 => b"\x1b[19~",
        67 => b"\x1b[20~",
        68 => b"\x1b[21~",
        87 => b"\x1b[23~",
        88 => b"\x1b[24~",
        102 => b"\x1b[1~",
        103 if application_cursor => b"\x1bOA",
        103 => b"\x1b[A",
        104 => b"\x1b[5~",
        105 if application_cursor => b"\x1bOD",
        105 => b"\x1b[D",
        106 if application_cursor => b"\x1bOC",
        106 => b"\x1b[C",
        107 => b"\x1b[4~",
        108 if application_cursor => b"\x1bOB",
        108 => b"\x1b[B",
        109 => b"\x1b[6~",
        110 => b"\x1b[2~",
        111 => b"\x1b[3~",
        _ => b"",
    };
    if !sequence.is_empty() {
        input.push(sequence);
        return;
    }
    let Some(mut character) = plain_key(code) else {
        return;
    };
    if character.is_ascii_alphabetic() {
        if state.shift != state.caps_lock {
            character = character.to_ascii_uppercase();
        }
    } else if state.shift {
        character = shifted_key(code).unwrap_or(character);
    }
    if state.control {
        character = control_character(character);
    }
    if state.alt {
        input.push(b"\x1b");
    }
    input.push(&[character]);
}

fn keypad_key(code: u16, application: bool, numeric: bool, newline: bool) -> Option<&'static [u8]> {
    Some(if application {
        match code {
            55 => b"\x1bOj",
            71 => b"\x1bOw",
            72 => b"\x1bOx",
            73 => b"\x1bOy",
            74 => b"\x1bOm",
            75 => b"\x1bOt",
            76 => b"\x1bOu",
            77 => b"\x1bOv",
            78 => b"\x1bOk",
            79 => b"\x1bOq",
            80 => b"\x1bOr",
            81 => b"\x1bOs",
            82 => b"\x1bOp",
            83 => b"\x1bOn",
            96 => b"\x1bOM",
            98 => b"\x1bOo",
            _ => return None,
        }
    } else if numeric {
        match code {
            55 => b"*",
            71 => b"7",
            72 => b"8",
            73 => b"9",
            74 => b"-",
            75 => b"4",
            76 => b"5",
            77 => b"6",
            78 => b"+",
            79 => b"1",
            80 => b"2",
            81 => b"3",
            82 => b"0",
            83 => b".",
            96 if newline => b"\r\n",
            96 => b"\r",
            98 => b"/",
            _ => return None,
        }
    } else {
        match code {
            55 => b"*",
            71 => b"\x1b[1~",
            72 => b"\x1b[A",
            73 => b"\x1b[5~",
            74 => b"-",
            75 => b"\x1b[D",
            76 => b"\x1b[G",
            77 => b"\x1b[C",
            78 => b"+",
            79 => b"\x1b[4~",
            80 => b"\x1b[B",
            81 => b"\x1b[6~",
            82 => b"\x1b[2~",
            83 => b"\x1b[3~",
            96 if newline => b"\r\n",
            96 => b"\r",
            98 => b"/",
            _ => return None,
        }
    })
}

fn control_character(character: u8) -> u8 {
    match character.to_ascii_lowercase() {
        b'@' | b' ' => 0,
        b'a'..=b'z' => character.to_ascii_lowercase() - b'a' + 1,
        b'[' => 0x1b,
        b'\\' => 0x1c,
        b']' => 0x1d,
        b'^' => 0x1e,
        b'_' => 0x1f,
        b'?' => 0x7f,
        _ => character,
    }
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

pub struct InputQueue {
    bytes: [u8; INPUT_CAPACITY],
    head: usize,
    length: usize,
}

impl InputQueue {
    pub const fn new() -> Self {
        Self {
            bytes: [0; INPUT_CAPACITY],
            head: 0,
            length: 0,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.length == 0
    }

    pub fn remaining(&self) -> usize {
        self.bytes.len() - self.length
    }

    pub fn push(&mut self, bytes: &[u8]) {
        assert!(bytes.len() <= self.remaining());
        for byte in bytes {
            let tail = (self.head + self.length) % self.bytes.len();
            self.bytes[tail] = *byte;
            self.length += 1;
        }
    }

    pub fn contiguous(&self) -> &[u8] {
        &self.bytes[self.head..self.head + self.length.min(self.bytes.len() - self.head)]
    }

    pub fn consume(&mut self, count: usize) {
        debug_assert!(count <= self.length);
        self.head = (self.head + count) % self.bytes.len();
        self.length -= count;
    }
}

pub fn flush_input(session: &mut PtySession, input: &mut InputQueue) {
    while !input.is_empty() {
        let bytes = input.contiguous();
        match session.write(bytes) {
            Ok(0) => return,
            Ok(count) => input.consume(count),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return,
        }
    }
}
