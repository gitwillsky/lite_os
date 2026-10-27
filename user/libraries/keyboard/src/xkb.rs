//! 自包含的 XKB keymap 文本（美式布局）：从本 crate 的键码表生成，不依赖客户端机器上的 xkeyboard-config。
//!
//! Wayland 合成器通过 `wl_keyboard.keymap` 把这段文本交给客户端，客户端用 libxkbcommon 编译。
//! 键名统一为 `<Knnn>`（nnn 是 evdev 键码，三位十进制），XKB 键码 = evdev 键码 + 8。

use std::fmt::Write;

use crate::{Modifiers, plain_key, shifted_key};

/// evdev 键码到 XKB 键码的固定偏移。
pub const KEYCODE_OFFSET: u32 = 8;
/// XKB 键码上限（`maximum = 255`），所以 evdev 键码至多 247。
const MAX_EVDEV: u16 = 247;

/// XKB 修饰键掩码位（`xkb_mod_index` 的标准顺序：Shift、Lock、Control、Mod1、Mod2、Mod3、Mod4）。
const SHIFT: u32 = 1 << 0;
const LOCK: u32 = 1 << 1;
const CONTROL: u32 = 1 << 2;
const MOD1: u32 = 1 << 3;
const MOD4: u32 = 1 << 6;

/// `wl_keyboard.modifiers` 的 `(depressed, locked)`；Caps Lock 是锁定态，其余是按住态。
pub fn modifier_masks(modifiers: Modifiers) -> (u32, u32) {
    let mut depressed = 0;
    for (active, mask) in [
        (modifiers.shift, SHIFT),
        (modifiers.control, CONTROL),
        (modifiers.alt, MOD1),
        (modifiers.super_key, MOD4),
    ] {
        if active {
            depressed |= mask;
        }
    }
    (depressed, if modifiers.caps { LOCK } else { 0 })
}

/// 非字符键：`(evdev 键码, 一档 keysym, 二档 keysym)`；没有二档的写 `None`。
const SPECIAL_KEYS: &[(u16, &str, Option<&str>)] = &[
    (1, "Escape", None),
    (14, "BackSpace", None),
    (15, "Tab", Some("ISO_Left_Tab")),
    (28, "Return", None),
    (29, "Control_L", None),
    (42, "Shift_L", None),
    (54, "Shift_R", None),
    (55, "KP_Multiply", None),
    (56, "Alt_L", None),
    (58, "Caps_Lock", None),
    (59, "F1", None),
    (60, "F2", None),
    (61, "F3", None),
    (62, "F4", None),
    (63, "F5", None),
    (64, "F6", None),
    (65, "F7", None),
    (66, "F8", None),
    (67, "F9", None),
    (68, "F10", None),
    (69, "Num_Lock", None),
    (70, "Scroll_Lock", None),
    (71, "KP_7", None),
    (72, "KP_8", None),
    (73, "KP_9", None),
    (74, "KP_Subtract", None),
    (75, "KP_4", None),
    (76, "KP_5", None),
    (77, "KP_6", None),
    (78, "KP_Add", None),
    (79, "KP_1", None),
    (80, "KP_2", None),
    (81, "KP_3", None),
    (82, "KP_0", None),
    (83, "KP_Decimal", None),
    (87, "F11", None),
    (88, "F12", None),
    (96, "KP_Enter", None),
    (97, "Control_R", None),
    (98, "KP_Divide", None),
    (99, "Print", None),
    (100, "Alt_R", None),
    (102, "Home", None),
    (103, "Up", None),
    (104, "Prior", None),
    (105, "Left", None),
    (106, "Right", None),
    (107, "End", None),
    (108, "Down", None),
    (109, "Next", None),
    (110, "Insert", None),
    (111, "Delete", None),
    (119, "Pause", None),
    (125, "Super_L", None),
    (126, "Super_R", None),
    (127, "Menu", None),
];

/// 修饰键的 `(键码, XKB 修饰符名)`，用于 `modifier_map`。
const MODIFIER_KEYS: &[(u16, &str)] = &[
    (42, "Shift"),
    (54, "Shift"),
    (58, "Lock"),
    (29, "Control"),
    (97, "Control"),
    (56, "Mod1"),
    (100, "Mod1"),
    (125, "Mod4"),
    (126, "Mod4"),
];

/// ASCII 字符的 XKB keysym 名。
fn keysym(byte: u8) -> &'static str {
    match byte {
        b'a'..=b'z' | b'0'..=b'9' => {
            // 字母和数字的 keysym 名就是字符本身；借用静态字符串表避免分配。
            const NAMES: &str = "abcdefghijklmnopqrstuvwxyz0123456789";
            let index = NAMES.find(byte as char).expect("alphanumeric");
            &NAMES[index..=index]
        }
        b' ' => "space",
        b'!' => "exclam",
        b'"' => "quotedbl",
        b'#' => "numbersign",
        b'$' => "dollar",
        b'%' => "percent",
        b'&' => "ampersand",
        b'\'' => "apostrophe",
        b'(' => "parenleft",
        b')' => "parenright",
        b'*' => "asterisk",
        b'+' => "plus",
        b',' => "comma",
        b'-' => "minus",
        b'.' => "period",
        b'/' => "slash",
        b':' => "colon",
        b';' => "semicolon",
        b'<' => "less",
        b'=' => "equal",
        b'>' => "greater",
        b'?' => "question",
        b'@' => "at",
        b'[' => "bracketleft",
        b'\\' => "backslash",
        b']' => "bracketright",
        b'^' => "asciicircum",
        b'_' => "underscore",
        b'`' => "grave",
        b'{' => "braceleft",
        b'|' => "bar",
        b'}' => "braceright",
        b'~' => "asciitilde",
        other => unreachable!("no keysym for {other:#x}"),
    }
}

fn key_name(code: u16) -> String {
    format!("<K{code:03}>")
}

/// 返回完整的 keymap 文本（`XKB_KEYMAP_FORMAT_TEXT_V1`）。
pub fn keymap_text() -> String {
    let mut text = String::from(
        "xkb_keymap {\nxkb_keycodes \"liteos\" {\n    minimum = 8;\n    maximum = 255;\n",
    );
    let codes = || (1..=MAX_EVDEV).filter(|code| key_symbols(*code).is_some());
    for code in codes() {
        let _ = writeln!(
            text,
            "    {} = {};",
            key_name(code),
            u32::from(code) + KEYCODE_OFFSET
        );
    }
    text.push_str(
        "};\nxkb_types \"liteos\" {\n    virtual_modifiers NumLock;\n\
    type \"ONE_LEVEL\" {\n        modifiers = none;\n        map[None] = Level1;\n        level_name[Level1] = \"Any\";\n    };\n\
    type \"TWO_LEVEL\" {\n        modifiers = Shift;\n        map[Shift] = Level2;\n        level_name[Level1] = \"Base\";\n        level_name[Level2] = \"Shift\";\n    };\n\
    type \"ALPHABETIC\" {\n        modifiers = Shift+Lock;\n        map[Shift] = Level2;\n        map[Lock] = Level2;\n        level_name[Level1] = \"Base\";\n        level_name[Level2] = \"Caps\";\n    };\n\
};\nxkb_compat \"liteos\" {\n    interpret.useModMapMods = AnyLevel;\n    interpret.repeat = False;\n\
    interpret Caps_Lock+AnyOf(all) {\n        action = LockMods(modifiers = Lock);\n    };\n\
    interpret Any+AnyOf(all) {\n        action = SetMods(modifiers = modMapMods, clearLocks);\n    };\n\
};\nxkb_symbols \"liteos\" {\n    name[group1] = \"English (US)\";\n",
    );
    for code in codes() {
        let (kind, levels) = key_symbols(code).expect("filtered above");
        let _ = writeln!(
            text,
            "    key {} {{ type = \"{kind}\", symbols[Group1] = [ {levels} ] }};",
            key_name(code)
        );
    }
    for modifier in ["Shift", "Lock", "Control", "Mod1", "Mod4"] {
        let keys: Vec<String> = MODIFIER_KEYS
            .iter()
            .filter(|(_, name)| *name == modifier)
            .map(|(code, _)| key_name(*code))
            .collect();
        let _ = writeln!(
            text,
            "    modifier_map {modifier} {{ {} }};",
            keys.join(", ")
        );
    }
    text.push_str("};\n};\n");
    text
}

/// 一个键的 `(类型, "一档, 二档")`；没有定义的键返回 `None`。
fn key_symbols(code: u16) -> Option<(&'static str, String)> {
    if let Some(plain) = plain_key(code) {
        if plain.is_ascii_lowercase() {
            let upper = plain.to_ascii_uppercase();
            return Some((
                "ALPHABETIC",
                format!("{}, {}", keysym(plain), keysym_upper(upper)),
            ));
        }
        return Some(match shifted_key(code) {
            Some(shifted) => (
                "TWO_LEVEL",
                format!("{}, {}", keysym(plain), keysym(shifted)),
            ),
            None => ("ONE_LEVEL", keysym(plain).to_owned()),
        });
    }
    SPECIAL_KEYS
        .iter()
        .find(|(special, ..)| *special == code)
        .map(|(_, first, second)| match second {
            Some(second) => ("TWO_LEVEL", format!("{first}, {second}")),
            None => ("ONE_LEVEL", (*first).to_owned()),
        })
}

/// 大写字母的 keysym 名（`Q` 而不是 `q`）。
fn keysym_upper(upper: u8) -> &'static str {
    const NAMES: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZ";
    let index = (upper - b'A') as usize;
    &NAMES[index..=index]
}

/// 将已验证 US keymap 的 XKB modifier masks 还原为共享键盘状态。
/// `depressed`/`latched`/`locked` 分别来自 wl_keyboard.modifiers；返回当前有效修饰键。
pub fn modifiers_from_masks(depressed: u32, latched: u32, locked: u32) -> Modifiers {
    let active = depressed | latched | locked;
    Modifiers {
        shift: active & SHIFT != 0,
        control: active & CONTROL != 0,
        alt: active & MOD1 != 0,
        super_key: active & MOD4 != 0,
        caps: active & LOCK != 0,
    }
}

/// US keymap 中允许重复的键；修饰键和 Caps Lock 不重复。
pub fn repeats(code: u32) -> bool {
    code <= u32::from(MAX_EVDEV) && !MODIFIER_KEYS.iter().any(|(key, _)| u32::from(*key) == code)
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod mask_tests {
    use super::*;
    #[test]
    fn modifier_projection_roundtrips_all_supported_combinations() {
        for bits in 0..64 {
            let state = Modifiers {
                shift: bits & 1 != 0,
                control: bits & 2 != 0,
                alt: bits & 4 != 0,
                super_key: bits & 8 != 0,
                caps: bits & 16 != 0,
            };
            let (depressed, locked) = modifier_masks(state);
            assert_eq!(modifiers_from_masks(depressed, 0, locked), state);
        }
        for code in [29, 42, 54, 56, 58, 97, 100, 125, 126] {
            assert!(!repeats(code));
        }
        for code in [14, 30, 103, 111] {
            assert!(repeats(code));
        }
    }
}
