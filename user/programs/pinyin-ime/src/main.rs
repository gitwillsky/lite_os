//! 独立的离线输入法。Wayland 连接只在已授权的会话内可用。

mod composition;
mod dictionary;

use canvas::{Rect, Surface};
use classic_theme::{Edge, SystemColor as C};
use classic_ui::{
    Display, EventKind, Ink, InputMethodEvent, KeyboardInteractivity, Layer, LayerSettings,
    Painter, Resources, WindowId, layer_anchor,
};
use composition::{Composition, PAGE};
use dictionary::Dictionary;
use keyboard::{Modifiers, character, xkb};
use std::{fs, io, process::ExitCode};

const POPUP_HEIGHT: i32 = 46;
const STATUS: Rect = Rect::new(0, 0, 22, 22);

#[derive(Default)]
struct Input {
    chinese: bool,
    ascii_punctuation: bool,
    composition: Composition,
    modifiers: Modifiers,
    held: Vec<u32>,
    swallowed: Vec<u32>,
    shift: Option<u32>,
    active: bool,
}

struct Outcome {
    handled: bool,
    update_preedit: bool,
    status_changed: bool,
    commit: Option<String>,
}

impl Outcome {
    const FORWARD: Self = Self {
        handled: false,
        update_preedit: false,
        status_changed: false,
        commit: None,
    };
    const CONSUME: Self = Self {
        handled: true,
        ..Self::FORWARD
    };
    const UPDATE: Self = Self {
        update_preedit: true,
        ..Self::CONSUME
    };
}

impl Input {
    fn toggle(&mut self) {
        self.chinese = !self.chinese;
        self.composition.clear();
    }

    fn activate(&mut self, active: bool) {
        self.active = active;
        self.composition.clear();
        self.held.clear();
        self.swallowed.clear();
        self.shift = None;
    }

    fn key(&mut self, key: u32, pressed: bool, dictionary: &Dictionary<'_>) -> Outcome {
        if !pressed {
            self.held.retain(|held| *held != key);
            if self.shift.take_if(|candidate| *candidate == key).is_some() {
                self.toggle();
                return Outcome {
                    status_changed: true,
                    update_preedit: true,
                    ..Outcome::FORWARD
                };
            }
            let swallowed = self.swallowed.iter().position(|held| *held == key);
            if let Some(index) = swallowed {
                self.swallowed.swap_remove(index);
                return Outcome::CONSUME;
            }
            return Outcome::FORWARD;
        }
        if matches!(key, 42 | 54)
            && self.held.is_empty()
            && !(self.modifiers.control || self.modifiers.alt || self.modifiers.super_key)
        {
            self.shift = Some(key);
        } else {
            self.shift = None;
        }
        if !self.held.contains(&key) {
            self.held.push(key);
        }

        let result = self.press(key, dictionary);
        if result.handled && !self.swallowed.contains(&key) {
            self.swallowed.push(key);
        }
        result
    }

    fn press(&mut self, key: u32, dictionary: &Dictionary<'_>) -> Outcome {
        if !self.chinese {
            return Outcome::FORWARD;
        }
        let mods = self.modifiers;
        if mods.control && !mods.shift && !mods.alt && !mods.super_key && key == 52 {
            self.ascii_punctuation = !self.ascii_punctuation;
            return Outcome::CONSUME;
        }
        if mods.control || mods.alt || mods.super_key {
            if self.composition.spell.is_empty() {
                return Outcome::FORWARD;
            }
            self.composition.clear();
            return Outcome {
                update_preedit: true,
                ..Outcome::FORWARD
            };
        }
        let character = u16::try_from(key).ok().and_then(|key| character(key, mods));
        let composing = !self.composition.spell.is_empty();
        if let Some(letter) =
            character.filter(|ch| ch.is_ascii_lowercase() || *ch == '\'' && composing)
        {
            self.composition.insert(letter, dictionary);
            return Outcome::UPDATE;
        }
        let commit = if composing {
            let model = &mut self.composition;
            match key {
                1 => {
                    model.clear();
                    return Outcome::UPDATE;
                }
                14 => {
                    model.backspace(dictionary);
                    return Outcome::UPDATE;
                }
                111 => {
                    model.delete(dictionary);
                    return Outcome::UPDATE;
                }
                105 => {
                    model.cursor = model.cursor.saturating_sub(1);
                    return Outcome::UPDATE;
                }
                106 => {
                    model.cursor = (model.cursor + 1).min(model.spell.len());
                    return Outcome::UPDATE;
                }
                103 => {
                    model.navigate(-1);
                    return Outcome::UPDATE;
                }
                108 => {
                    model.navigate(1);
                    return Outcome::UPDATE;
                }
                104 | 12 => {
                    model.navigate(-(PAGE as isize));
                    return Outcome::UPDATE;
                }
                109 | 13 => {
                    model.navigate(PAGE as isize);
                    return Outcome::UPDATE;
                }
                28 => Some(model.raw()),
                57 => model
                    .confirm(model.position(), dictionary)
                    .or_else(|| Some(model.raw())),
                2..=6 if !mods.shift => model.confirm(key as usize - 2, dictionary),
                _ if matches!(key, 42 | 54 | 29 | 97 | 56 | 100 | 125 | 126) => {
                    return Outcome::FORWARD;
                }
                _ => {
                    if let Some(mark) =
                        character.and_then(|ch| punctuation(ch, self.ascii_punctuation))
                    {
                        let mut value = model
                            .confirm(model.position(), dictionary)
                            .unwrap_or_default();
                        value.push_str(&model.raw());
                        value.push(mark);
                        Some(value)
                    } else {
                        model.clear();
                        return Outcome {
                            update_preedit: true,
                            ..Outcome::FORWARD
                        };
                    }
                }
            }
        } else {
            let Some(mark) = character.and_then(|ch| punctuation(ch, self.ascii_punctuation))
            else {
                return Outcome::FORWARD;
            };
            Some(mark.to_string())
        };
        Outcome {
            commit,
            ..Outcome::UPDATE
        }
    }
}

fn punctuation(ch: char, ascii: bool) -> Option<char> {
    if ascii {
        return None;
    }
    Some(match ch {
        ',' => '，',
        '.' => '。',
        '?' => '？',
        '!' => '！',
        ';' => '；',
        ':' => '：',
        '(' => '（',
        ')' => '）',
        '[' => '【',
        ']' => '】',
        '<' => '《',
        '>' => '》',
        _ => return None,
    })
}

fn render(
    display: &mut Display,
    resources: &mut Resources,
    window: WindowId,
    draw: impl FnOnce(&mut Painter<'_>, Rect),
) -> io::Result<bool> {
    if !display.ready_to_draw(window) {
        return Ok(false);
    }
    let fonts = resources.fonts(display.configuration(window).expect("ready surface").scale)?;
    display.paint_frame(window, |pixels, configuration, _, _| {
        let (width, height) = configuration.pixel_size()?;
        let surface = Surface::new(pixels, width, height, width as usize)
            .ok_or(io::ErrorKind::InvalidData)?;
        let mut painter = Painter::new(surface, resources.theme(), &fonts, configuration.scale);
        draw(
            &mut painter,
            Rect::new(0, 0, configuration.width, configuration.height),
        );
        Ok(vec![Rect::new(0, 0, width, height)])
    })?;
    Ok(true)
}

fn popup_geometry(width: i32, index: usize) -> Rect {
    let pitch = (width - 10) / PAGE as i32;
    Rect::new(5 + index as i32 * pitch, 23, pitch, 18)
}

fn paint_popup(painter: &mut Painter<'_>, rect: Rect, composition: &Composition) {
    painter.fill(rect, C::Window);
    painter.edge(rect, Edge::Raised);
    painter.text_fit(
        6,
        4,
        rect.width - 12,
        &composition.spell,
        Ink::Color(C::WindowText),
        false,
    );
    let prefix = &composition.spell[..composition.cursor];
    let caret = (painter.text_width(prefix, false) / painter.scale()).min(rect.width - 12);
    painter.fill(Rect::new(6 + caret, 5, 1, 14), C::WindowText);
    for (index, candidate) in composition.page().iter().enumerate() {
        let slot = popup_geometry(rect.width, index);
        let selected = index == composition.position();
        if selected {
            painter.fill(slot, C::Hilight);
        }
        painter.text_fit(
            slot.x + 3,
            slot.y + 1,
            slot.width - 4,
            &format!("{} {}", index + 1, candidate.text),
            Ink::Color(if selected {
                C::HilightText
            } else {
                C::WindowText
            }),
            selected,
        );
    }
}

fn run() -> io::Result<()> {
    let table = fs::read_to_string("/usr/share/liteos/pinyin.tsv")?;
    let dictionary = Dictionary::new(&table);
    let mut resources = Resources::system()?;
    let mut display = Display::connect()?;
    let status = display.create_layer(
        "liteos-ime-status",
        LayerSettings {
            layer: Layer::Overlay,
            width: 22,
            height: 22,
            anchor: layer_anchor::BOTTOM | layer_anchor::RIGHT,
            margin: [0, 4, 2, 0],
            exclusive_zone: -1,
            keyboard: KeyboardInteractivity::None,
        },
    )?;
    display.start_input_method(status)?;
    let mut input = Input::default();
    let mut popup = None;
    let mut status_dirty = true;
    let mut popup_dirty = false;
    let mut pointer = (0, 0);
    loop {
        if status_dirty
            && render(&mut display, &mut resources, status, |painter, rect| {
                painter.fill(rect, C::ButtonFace);
                let inner = painter.edge(STATUS.inset(1), Edge::Raised);
                if input.chinese {
                    painter.fill(inner, C::Hilight);
                }
                painter.text_centered(
                    inner,
                    if input.chinese { "中" } else { "英" },
                    Ink::Color(if input.chinese {
                        C::HilightText
                    } else {
                        C::ButtonText
                    }),
                    true,
                );
            })?
        {
            status_dirty = false;
        }

        if input.composition.spell.is_empty() || !input.active {
            if let Some(window) = popup.take() {
                display.destroy_window(window)?;
            }
        } else {
            if popup.is_none() {
                let width = display
                    .screen_size()
                    .map_or(518, |(w, _)| w.clamp(100, 518));
                popup = Some(display.create_input_popup((width, POPUP_HEIGHT))?);
                popup_dirty = true;
            }
            if let Some(window) = popup.filter(|_| popup_dirty)
                && render(&mut display, &mut resources, window, |p, r| {
                    paint_popup(p, r, &input.composition)
                })?
            {
                popup_dirty = false;
            }
        }
        let event = display.next_event()?;
        let mut forward = None;
        let outcome = match event.kind {
            EventKind::InputMethod(InputMethodEvent::Active(active)) => {
                input.activate(active);
                Outcome::CONSUME
            }
            EventKind::InputMethod(
                value @ InputMethodEvent::Modifiers {
                    depressed,
                    latched,
                    locked,
                    ..
                },
            ) => {
                input.modifiers = xkb::modifiers_from_masks(depressed, latched, locked);
                forward = Some(value);
                Outcome::FORWARD
            }
            EventKind::InputMethod(value @ InputMethodEvent::Key { key, pressed, .. })
                if input.active =>
            {
                forward = Some(value);
                input.key(key, pressed, &dictionary)
            }
            EventKind::Configure(_) => {
                status_dirty |= event.window == status;
                popup_dirty |= Some(event.window) == popup;
                Outcome::CONSUME
            }
            EventKind::Button {
                code: 0x110,
                pressed: false,
                ..
            } if event.window == status => {
                input.toggle();
                Outcome {
                    status_changed: true,
                    update_preedit: true,
                    ..Outcome::CONSUME
                }
            }
            EventKind::PointerEnter { x, y } | EventKind::PointerMove { x, y } => {
                pointer = (x as i32, y as i32);
                Outcome::CONSUME
            }
            EventKind::Button {
                code: 0x110,
                pressed: false,
                ..
            } if Some(event.window) == popup => {
                let width = display.configuration(event.window).map_or(0, |c| c.width);
                let commit = (0..PAGE).find_map(|index| {
                    let r = popup_geometry(width, index);
                    ((r.x..r.right()).contains(&pointer.0)
                        && (r.y..r.bottom()).contains(&pointer.1))
                    .then(|| input.composition.confirm(index, &dictionary))
                    .flatten()
                });
                Outcome {
                    commit,
                    ..Outcome::UPDATE
                }
            }
            _ => Outcome::CONSUME,
        };
        status_dirty |= outcome.status_changed;
        if outcome.update_preedit {
            popup_dirty = true;
            if input.active {
                display.commit_input_method(
                    &input.composition.spell,
                    input.composition.cursor,
                    outcome.commit.as_deref(),
                )?;
            }
        }
        if !outcome.handled
            && let Some(value) = forward
        {
            display.forward_input_method_event(&value)?;
        }
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("pinyin-ime: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shift_alone_switches_language_without_eating_shift_chords() {
        let dictionary = Dictionary::new("ni\t你\t100\n");
        let mut input = Input::default();
        input.modifiers.shift = true;
        assert!(!input.key(42, true, &dictionary).handled);
        assert!(!input.key(49, true, &dictionary).handled);
        input.key(49, false, &dictionary);
        assert!(!input.key(42, false, &dictionary).status_changed);
        input.key(42, true, &dictionary);
        assert!(input.key(42, false, &dictionary).status_changed);
        input.modifiers.shift = false;
        assert!(input.key(49, true, &dictionary).handled);
        assert!(input.key(49, false, &dictionary).handled);
        input.key(23, true, &dictionary);
        assert_eq!(
            input.key(57, true, &dictionary).commit.as_deref(),
            Some("你")
        );
        assert!(input.composition.spell.is_empty());
    }
}
