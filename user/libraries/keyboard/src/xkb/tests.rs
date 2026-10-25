use super::{KEYCODE_OFFSET, SPECIAL_KEYS, key_symbols, keymap_text, modifier_masks};
use crate::{Modifiers, plain_key};

#[test]
fn every_printable_key_of_the_shared_table_is_in_the_keymap() {
    let text = keymap_text();
    for code in 1..=247u16 {
        if plain_key(code).is_some() {
            assert!(
                text.contains(&format!(
                    "<K{code:03}> = {};",
                    u32::from(code) + KEYCODE_OFFSET
                )),
                "{code}"
            );
            assert!(text.contains(&format!("key <K{code:03}> {{")), "{code}");
        }
    }
    assert!(text.contains("key <K016> { type = \"ALPHABETIC\", symbols[Group1] = [ q, Q ] };"));
    assert!(text.contains("key <K002> { type = \"TWO_LEVEL\", symbols[Group1] = [ 1, exclam ] };"));
    assert!(text.contains("key <K057> { type = \"ONE_LEVEL\", symbols[Group1] = [ space ] };"));
    assert!(
        text.contains(
            "key <K015> { type = \"TWO_LEVEL\", symbols[Group1] = [ Tab, ISO_Left_Tab ] };"
        )
    );
    assert!(
        text.contains("key <K043> { type = \"TWO_LEVEL\", symbols[Group1] = [ backslash, bar ] };")
    );
}

#[test]
fn key_names_and_codes_are_unique_and_inside_the_xkb_range() {
    let text = keymap_text();
    let mut seen = std::collections::HashSet::new();
    for line in text.lines().filter(|line| line.starts_with("    <K")) {
        let (name, code) = line.trim().trim_end_matches(';').split_once(" = ").unwrap();
        assert!(seen.insert(name.to_owned()), "duplicate {name}");
        let code: u32 = code.parse().unwrap();
        assert!((8..=255).contains(&code), "{line}");
    }
    assert!(seen.len() > 100, "{}", seen.len());
    let mut specials: Vec<u16> = SPECIAL_KEYS.iter().map(|(code, ..)| *code).collect();
    specials.sort_unstable();
    specials.dedup();
    assert_eq!(
        specials.len(),
        SPECIAL_KEYS.len(),
        "a special key is listed twice"
    );
    assert!(
        specials.iter().all(|code| plain_key(*code).is_none()),
        "special keys must not shadow printable ones"
    );
}

#[test]
fn the_text_is_structurally_balanced_and_complete() {
    let text = keymap_text();
    assert_eq!(text.matches('{').count(), text.matches('}').count());
    assert_eq!(text.matches('[').count(), text.matches(']').count());
    for section in ["xkb_keycodes", "xkb_types", "xkb_compat", "xkb_symbols"] {
        assert_eq!(text.matches(section).count(), 1, "{section}");
    }
    assert!(text.starts_with("xkb_keymap {") && text.ends_with("};\n};\n"));
    assert!(text.is_ascii());
    for modifier_map in [
        "modifier_map Shift { <K042>, <K054> };",
        "modifier_map Lock { <K058> };",
        "modifier_map Control { <K029>, <K097> };",
        "modifier_map Mod1 { <K056>, <K100> };",
        "modifier_map Mod4 { <K125>, <K126> };",
    ] {
        assert!(text.contains(modifier_map), "{modifier_map}");
    }
}

#[test]
fn every_modifier_key_has_a_symbol() {
    for (code, _) in super::MODIFIER_KEYS {
        assert!(key_symbols(*code).is_some(), "{code}");
    }
}

#[test]
fn modifier_masks_use_the_standard_xkb_bits() {
    assert_eq!(modifier_masks(Modifiers::default()), (0, 0));
    let all = Modifiers {
        shift: true,
        control: true,
        alt: true,
        super_key: true,
        caps: true,
    };
    assert_eq!(modifier_masks(all), (0b100_1101, 0b10));
    let shift = Modifiers {
        shift: true,
        ..Modifiers::default()
    };
    assert_eq!(modifier_masks(shift), (1, 0));
}
