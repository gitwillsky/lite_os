//! Shared evdev keycode translation. Owns the single keycode→character table
//! so the terminal (which turns keys into PTY bytes) and the UI text-input
//! primitive (which turns keys into editing intents) never carry rival maps.

/// Latched modifier state accumulated from key press/release events.
#[derive(Clone, Copy, Default)]
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
/// renderer maps keys to these and lets the field's `onInput`/`onKeyDown` React
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
