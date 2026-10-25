//! Shared evdev keycode translation. Owns the single keycode→character table so the terminal
//! (PTY bytes, for terminal clients) and the UI text-input
//! primitive (editing intents) never carry rival maps.

extern crate liteos_runtime as _;
pub mod xkb;

/// Latched modifier state accumulated from key press/release events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Modifiers {
    pub shift: bool,
    pub control: bool,
    pub alt: bool,
    pub super_key: bool,
    pub caps: bool,
}

impl Modifiers {
    /// Folds one key event into the latched state, returning whether `code` was
    /// a modifier key (so the caller can skip producing text for it). Caps lock
    /// toggles on the press edge (`value == 1`) only.
    pub fn apply(&mut self, code: u32, value: i32) -> bool {
        let pressed = value != 0;
        match code {
            42 | 54 => self.shift = pressed,
            29 | 97 => self.control = pressed,
            56 | 100 => self.alt = pressed,
            125 | 126 => self.super_key = pressed,
            // Caps lock latches on the press edge; its release is still a
            // modifier key (produces no text), so it falls through to `true`.
            58 => {
                if value == 1 {
                    self.caps = !self.caps;
                }
            }
            _ => return false,
        }
        true
    }
}

/// Unshifted ASCII for a printable evdev keycode, or `None` for non-printable
/// keys (function keys, navigation, modifiers).
pub fn plain_key(code: u16) -> Option<u8> {
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

/// Shifted ASCII for the symbol keys whose shifted form is not just uppercase.
pub fn shifted_key(code: u16) -> Option<u8> {
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

/// Resolves a printable keycode to its final character under shift/caps, or
/// `None` for non-printable keys. Alphabetic keys uppercase when exactly one of
/// shift/caps is active; symbol keys use `shifted_key` under shift.
pub fn character(code: u16, modifiers: Modifiers) -> Option<char> {
    let mut byte = plain_key(code)?;
    if byte.is_ascii_alphabetic() {
        if modifiers.shift != modifiers.caps {
            byte.make_ascii_uppercase();
        }
    } else if modifiers.shift {
        byte = shifted_key(code).unwrap_or(byte);
    }
    Some(byte as char)
}

/// One editing intent produced from a key event for a focused text field. The
/// client maps keys to these and lets the field's input/key
/// handlers apply them (standard controlled-input).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextEdit {
    /// Append this character to the value.
    Insert(char),
    /// Delete the character before the caret.
    Backspace,
}

/// Maps a key press to a `TextEdit` for a focused text field, or `None` when
/// the key is a modifier, a release, or a non-text control key (Enter/Esc/
/// arrows — those are delivered to the field's `onKeyDown` instead).
pub fn text_edit(code: u32, value: i32, modifiers: Modifiers) -> Option<TextEdit> {
    if value == 0 {
        return None;
    }
    match code {
        14 => Some(TextEdit::Backspace),
        _ => character(code as u16, modifiers).map(TextEdit::Insert),
    }
}

/// Translates one key event into the PTY byte sequence a terminal receives (`None` for modifier,
/// release and unmapped keys). `application_cursor_keys` selects DECCKM `ESC O x` navigation.
pub fn terminal_bytes(
    state: &mut Modifiers,
    code: u32,
    value: i32,
    application_cursor_keys: bool,
) -> Option<Vec<u8>> {
    let pressed = value != 0;
    // 修饰键折叠交给本 crate 的 keymap，避免与 UI 文本输入各持一份键码表。
    if state.apply(code, value) || !pressed {
        return None;
    }
    terminal_input(
        code,
        pressed,
        *state,
        character(code as u16, *state),
        application_cursor_keys,
    )
}

/// 将已提交的 Wayland/XKB Unicode 文本和物理功能键编码为终端输入。
/// 修饰状态由输入服务提供，不再次按键帽推算字符；释放事件及无文本键返回 None。
/// DECCKM 仅影响导航转义序列，不改变提交文字的 UTF-8。
pub fn terminal_input(
    code: u32,
    pressed: bool,
    state: Modifiers,
    text: Option<char>,
    application_cursor_keys: bool,
) -> Option<Vec<u8>> {
    if !pressed {
        return None;
    }
    let special: Option<&[u8]> = match code {
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
    let character = text?;
    let mut bytes = Vec::with_capacity(5);
    if state.alt {
        bytes.push(0x1b);
    }
    if state.control && character.is_ascii() {
        let value = character.to_ascii_uppercase() as u8;
        bytes.push(match value {
            b' ' | b'@' | b'2' => 0,
            b'A'..=b'Z' | b'['..=b'_' => value & 0x1f,
            b'?' | b'8' => 0x7f,
            _ => value,
        });
    } else {
        bytes.extend_from_slice(character.encode_utf8(&mut [0; 4]).as_bytes());
    }
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::{Modifiers, TextEdit, character, text_edit};

    #[test]
    fn shift_and_caps_uppercase_letters_exclusively() {
        let none = Modifiers::default();
        assert_eq!(character(30, none), Some('a')); // KEY_A
        let shift = Modifiers {
            shift: true,
            ..none
        };
        assert_eq!(character(30, shift), Some('A'));
        let caps = Modifiers { caps: true, ..none };
        assert_eq!(character(30, caps), Some('A'));
        let both = Modifiers {
            shift: true,
            caps: true,
            ..none
        };
        assert_eq!(character(30, both), Some('a'));
    }

    #[test]
    fn symbols_use_the_shifted_table() {
        let shift = Modifiers {
            shift: true,
            ..Modifiers::default()
        };
        assert_eq!(character(2, shift), Some('!')); // KEY_1 -> !
        assert_eq!(character(2, Modifiers::default()), Some('1'));
    }

    #[test]
    fn text_edit_maps_backspace_and_ignores_releases() {
        let none = Modifiers::default();
        assert_eq!(text_edit(14, 1, none), Some(TextEdit::Backspace));
        assert_eq!(text_edit(30, 1, none), Some(TextEdit::Insert('a')));
        assert_eq!(text_edit(30, 0, none), None); // release
        assert_eq!(text_edit(28, 1, none), None); // Enter is not a text edit
    }

    #[test]
    fn modifier_keys_are_folded_not_emitted() {
        let mut modifiers = Modifiers::default();
        assert!(modifiers.apply(42, 1)); // left shift down
        assert!(modifiers.shift);
        assert!(!modifiers.apply(30, 1)); // a letter is not a modifier
        assert!(modifiers.apply(42, 0)); // shift up
        assert!(!modifiers.shift);
        assert!(modifiers.apply(125, 1)); // left super down
        assert!(modifiers.super_key);
        assert!(modifiers.apply(125, 0));
        assert!(!modifiers.super_key);
    }
}

#[cfg(test)]
mod terminal_tests {
    use super::{Modifiers, terminal_bytes};

    #[test]
    fn normal_cursor_mode_keeps_csi_navigation_encoding() {
        for (code, expected) in [
            (103, b"\x1b[A".as_slice()),
            (105, b"\x1b[D".as_slice()),
            (106, b"\x1b[C".as_slice()),
            (108, b"\x1b[B".as_slice()),
        ] {
            assert_eq!(
                terminal_bytes(&mut Modifiers::default(), code, 1, false).as_deref(),
                Some(expected),
            );
        }
    }

    #[test]
    fn control_letters_and_alt_prefix_follow_pty_conventions() {
        let mut modifiers = Modifiers::default();
        assert!(terminal_bytes(&mut modifiers, 29, 1, false).is_none()); // left control down
        assert_eq!(terminal_bytes(&mut modifiers, 46, 1, false), Some(vec![3])); // Ctrl-C
        terminal_bytes(&mut modifiers, 29, 0, false);
        terminal_bytes(&mut modifiers, 56, 1, false); // left alt down
        assert_eq!(
            terminal_bytes(&mut modifiers, 30, 1, false),
            Some(vec![0x1b, b'a'])
        );
    }
}
