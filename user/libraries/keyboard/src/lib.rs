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

/// Encodes an evdev press and its Wayland/XKB text using xterm input sequences.
/// The two application modes select SS3 cursor or keypad sequences. Unmapped keys
/// and desktop shortcuts using Super return `None`.
pub fn terminal_input(
    code: u32,
    state: Modifiers,
    text: Option<char>,
    application_cursor: bool,
    application_keypad: bool,
) -> Option<Vec<u8>> {
    if state.super_key {
        return None;
    }
    let modifier =
        1 + u8::from(state.shift) + 2 * u8::from(state.alt) + 4 * u8::from(state.control);
    let keypad = match code {
        55 => Some((b'j', '*')),
        71 => Some((b'w', '7')),
        72 => Some((b'x', '8')),
        73 => Some((b'y', '9')),
        74 => Some((b'm', '-')),
        75 => Some((b't', '4')),
        76 => Some((b'u', '5')),
        77 => Some((b'v', '6')),
        78 => Some((b'k', '+')),
        79 => Some((b'q', '1')),
        80 => Some((b'r', '2')),
        81 => Some((b's', '3')),
        82 => Some((b'p', '0')),
        83 => Some((b'n', '.')),
        96 => Some((b'M', '\r')),
        98 => Some((b'o', '/')),
        117 => Some((b'X', '=')),
        _ => None,
    };
    if let Some((key, _)) = keypad
        && application_keypad
        && modifier == 1
    {
        return Some(vec![0x1b, b'O', key]);
    }

    let final_byte = match code {
        102 => Some(b'H'),
        103 => Some(b'A'),
        105 => Some(b'D'),
        106 => Some(b'C'),
        107 => Some(b'F'),
        108 => Some(b'B'),
        59..=62 => Some(b'P' + (code - 59) as u8),
        _ => None,
    };
    if let Some(key) = final_byte {
        return Some(if modifier != 1 {
            format!("\x1b[1;{modifier}{}", char::from(key)).into_bytes()
        } else {
            vec![
                0x1b,
                if application_cursor || (59..=62).contains(&code) {
                    b'O'
                } else {
                    b'['
                },
                key,
            ]
        });
    }

    let numbered = match code {
        110 => Some(2),
        111 => Some(3),
        104 => Some(5),
        109 => Some(6),
        63..=68 => Some([15, 17, 18, 19, 20, 21][(code - 63) as usize]),
        87 => Some(23),
        88 => Some(24),
        _ => None,
    };
    if let Some(key) = numbered {
        return Some(if modifier == 1 {
            format!("\x1b[{key}~").into_bytes()
        } else {
            format!("\x1b[{key};{modifier}~").into_bytes()
        });
    }

    if code == 15 && state.shift {
        return Some(b"\x1b[Z".to_vec());
    }
    let character = match code {
        1 => '\x1b',
        14 => {
            if state.control {
                '\x08'
            } else {
                '\x7f'
            }
        }
        15 => '\t',
        28 | 96 => '\r',
        _ => text.or_else(|| keypad.map(|(_, character)| character))?,
    };
    let mut bytes = Vec::with_capacity(5);
    if state.alt {
        bytes.push(0x1b);
    }
    if state.control && character.is_ascii() {
        let value = character.to_ascii_uppercase() as u8;
        bytes.push(match value {
            b' ' | b'@' | b'2' => 0,
            b'A'..=b'Z' | b'['..=b'_' => value & 0x1f,
            b'3'..=b'7' => value - b'3' + 0x1b,
            b'/' => 0x1f,
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
    use super::{Modifiers, terminal_input};

    #[test]
    fn xterm_cursor_key_modes_and_modifiers() {
        let plain = Modifiers::default();
        let shift = Modifiers {
            shift: true,
            ..plain
        };
        assert_eq!(
            terminal_input(105, plain, None, false, false).as_deref(),
            Some(&b"\x1b[D"[..])
        );
        assert_eq!(
            terminal_input(105, plain, None, true, false).as_deref(),
            Some(&b"\x1bOD"[..])
        );
        assert_eq!(
            terminal_input(105, shift, None, true, false).as_deref(),
            Some(&b"\x1b[1;2D"[..])
        );
    }

    #[test]
    fn xterm_function_and_keypad_encodings_match_terminfo() {
        let plain = Modifiers::default();
        assert_eq!(
            terminal_input(59, plain, None, false, false).as_deref(),
            Some(&b"\x1bOP"[..])
        );
        assert_eq!(
            terminal_input(88, plain, None, false, false).as_deref(),
            Some(&b"\x1b[24~"[..])
        );
        assert_eq!(
            terminal_input(96, plain, None, false, true).as_deref(),
            Some(&b"\x1bOM"[..])
        );
    }

    #[test]
    fn control_letters_and_alt_prefix_follow_pty_conventions() {
        let control = Modifiers {
            control: true,
            ..Modifiers::default()
        };
        let alt = Modifiers {
            alt: true,
            ..Modifiers::default()
        };
        assert_eq!(
            terminal_input(46, control, Some('c'), false, false),
            Some(vec![3])
        );
        assert_eq!(
            terminal_input(30, alt, Some('a'), false, false),
            Some(vec![0x1b, b'a'])
        );
    }
}
